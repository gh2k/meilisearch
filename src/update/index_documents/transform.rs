use std::borrow::Cow;
use std::convert::TryFrom;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use anyhow::{anyhow, Context};
use fst::{IntoStreamer, Streamer};
use grenad::CompressionType;
use roaring::RoaringBitmap;

use crate::{BEU32, Index, FieldsIdsMap};
use crate::update::AvailableDocumentsIds;
use super::merge_function::merge_two_obkvs;
use super::{create_writer, create_sorter, IndexDocumentsMethod};

pub struct TransformOutput {
    pub primary_key: u8,
    pub fields_ids_map: FieldsIdsMap,
    pub users_ids_documents_ids: fst::Map<Vec<u8>>,
    pub new_documents_ids: RoaringBitmap,
    pub replaced_documents_ids: RoaringBitmap,
    pub documents_count: usize,
    pub documents_file: File,
}

pub struct Transform<'t, 'i> {
    pub rtxn: &'t heed::RoTxn<'i>,
    pub index: &'i Index,
    pub chunk_compression_type: CompressionType,
    pub chunk_compression_level: Option<u32>,
    pub chunk_fusing_shrink_size: Option<u64>,
    pub max_nb_chunks: Option<usize>,
    pub max_memory: Option<usize>,
    pub index_documents_method: IndexDocumentsMethod,
}

impl Transform<'_, '_> {
    /// Extract the users ids, deduplicate and compute the new internal documents ids
    /// and fields ids, writing all the documents under their internal ids into a final file.
    ///
    /// Outputs the new `FieldsIdsMap`, the new `UsersIdsDocumentsIds` map, the new documents ids,
    /// the replaced documents ids, the number of documents in this update and the file
    /// containing all those documents.
    pub fn from_csv<R: Read>(self, reader: R) -> anyhow::Result<TransformOutput> {
        let mut fields_ids_map = self.index.fields_ids_map(self.rtxn)?;
        let documents_ids = self.index.documents_ids(self.rtxn)?;
        let mut available_documents_ids = AvailableDocumentsIds::from_documents_ids(&documents_ids);
        let users_ids_documents_ids = self.index.users_ids_documents_ids(self.rtxn).unwrap();

        let mut csv = csv::Reader::from_reader(reader);
        let headers = csv.headers()?;
        let primary_key = self.index.primary_key(self.rtxn)?;

        // Generate the new fields ids based on the current fields ids and this CSV headers.
        let mut fields_ids = Vec::new();
        for (i, header) in headers.iter().enumerate() {
            let id = fields_ids_map.insert(header).context("field id limit reached)")?;
            fields_ids.push((id, i));
        }

        // Extract the position of the primary key in the current headers, None if not found.
        let user_id_pos = match primary_key {
            Some(primary_key) => {
                // Te primary key have is known so we must find the position in the CSV headers.
                let name = fields_ids_map.name(primary_key).expect("found the primary key name");
                headers.iter().position(|h| h == name)
            },
            None => headers.iter().position(|h| h.contains("id")),
        };

        // Returns the field id in the fileds ids map, create an "id" field
        // in case it is not in the current headers.
        let primary_key_field_id = match user_id_pos {
            Some(pos) => fields_ids_map.id(&headers[pos]).expect("found the primary key"),
            None => {
                let id = fields_ids_map.insert("id").context("field id limit reached")?;
                // We make sure to add the primary key field id to the fields ids,
                // this way it is added to the obks.
                fields_ids.push((id, usize::max_value()));
                id
            },
        };

        // We sort the fields ids by the fields ids map id, this way we are sure to iterate over
        // the records fields in the fields ids map order and correctly generate the obkv.
        fields_ids.sort_unstable_by_key(|(field_id, _)| *field_id);

        /// Only the last value associated with an id is kept.
        fn keep_latest_obkv(_key: &[u8], obkvs: &[Cow<[u8]>]) -> anyhow::Result<Vec<u8>> {
            obkvs.last().context("no last value").map(|last| last.clone().into_owned())
        }

        // We initialize the sorter with the user indexing settings.
        let mut sorter = create_sorter(
            keep_latest_obkv,
            self.chunk_compression_type,
            self.chunk_compression_level,
            self.chunk_fusing_shrink_size,
            self.max_nb_chunks,
            self.max_memory,
        );

        // We write into the sorter to merge and deduplicate the documents
        // based on the users ids.
        let mut json_buffer = Vec::new();
        let mut obkv_buffer = Vec::new();
        let mut uuid_buffer = [0; uuid::adapter::Hyphenated::LENGTH];
        let mut record = csv::StringRecord::new();
        while csv.read_record(&mut record)? {

            obkv_buffer.clear();
            let mut writer = obkv::KvWriter::new(&mut obkv_buffer);

            // We extract the user id if we know where it is or generate an UUID V4 otherwise.
            // TODO we must validate the user id (i.e. [a-zA-Z0-9\-_]).
            let user_id = match user_id_pos {
                Some(pos) => &record[pos],
                None => uuid::Uuid::new_v4().to_hyphenated().encode_lower(&mut uuid_buffer),
            };

            // When the primary_key_field_id is found in the fields ids list
            // we return the generated document id instead of the record field.
            let iter = fields_ids.iter()
                .map(|(fi, i)| {
                    let field = if *fi == primary_key_field_id { user_id } else { &record[*i] };
                    (fi, field)
                });

            // We retrieve the field id based on the fields ids map fields ids order.
            for (field_id, field) in iter {
                // We serialize the attribute values as JSON strings.
                json_buffer.clear();
                serde_json::to_writer(&mut json_buffer, &field)?;
                writer.insert(*field_id, &json_buffer)?;
            }

            // We use the extracted/generated user id as the key for this document.
            sorter.insert(user_id, &obkv_buffer)?;
        }

        // Once we have sort and deduplicated the documents we write them into a final file.
        let mut final_sorter = create_sorter(
            |_docid, _obkvs| Err(anyhow!("cannot merge two documents")),
            self.chunk_compression_type,
            self.chunk_compression_level,
            self.chunk_fusing_shrink_size,
            self.max_nb_chunks,
            self.max_memory,
        );
        let mut new_users_ids_documents_ids_builder = fst::MapBuilder::memory();
        let mut replaced_documents_ids = RoaringBitmap::new();
        let mut new_documents_ids = RoaringBitmap::new();

        // While we write into final file we get or generate the internal documents ids.
        let mut documents_count = 0;
        let mut iter = sorter.into_iter()?;
        while let Some((user_id, update_obkv)) = iter.next()? {

            let (docid, obkv) = match users_ids_documents_ids.get(user_id) {
                Some(docid) => {
                    // If we find the user id in the current users ids documents ids map
                    // we use it and insert it in the list of replaced documents.
                    let docid = u32::try_from(docid).expect("valid document id");
                    replaced_documents_ids.insert(docid);

                    // Depending on the update indexing method we will merge
                    // the document update with the current document or not.
                    match self.index_documents_method {
                        IndexDocumentsMethod::ReplaceDocuments => (docid, update_obkv),
                        IndexDocumentsMethod::UpdateDocuments => {
                            let key = BEU32::new(docid);
                            let base_obkv = self.index.documents.get(&self.rtxn, &key)?
                                .context("document not found")?;
                            let update_obkv = obkv::KvReader::new(update_obkv);
                            merge_two_obkvs(base_obkv, update_obkv, &mut obkv_buffer);
                            (docid, obkv_buffer.as_slice())
                        }
                    }
                },
                None => {
                    // If this user id is new we add it to the users ids documents ids map
                    // for new ids and into the list of new documents.
                    let new_docid = available_documents_ids.next()
                        .context("no more available documents ids")?;
                    new_users_ids_documents_ids_builder.insert(user_id, new_docid as u64)?;
                    new_documents_ids.insert(new_docid);
                    (new_docid, update_obkv)
                },
            };

            // We insert the document under the documents ids map into the final file.
            final_sorter.insert(docid.to_be_bytes(), obkv)?;
            documents_count += 1;
        }

        // We create a final writer to write the new documents in order from the sorter.
        let file = tempfile::tempfile()?;
        let mut writer = create_writer(self.chunk_compression_type, self.chunk_compression_level, file)?;

        // Once we have written all the documents into the final sorter, we write the documents
        // into this writer, extract the file and reset the seek to be able to read it again.
        final_sorter.write_into(&mut writer)?;
        let mut documents_file = writer.into_inner()?;
        documents_file.seek(SeekFrom::Start(0))?;

        // We create the union between the existing users ids documents ids with the new ones.
        let new_users_ids_documents_ids = new_users_ids_documents_ids_builder.into_map();
        let union_ = fst::map::OpBuilder::new()
            .add(&users_ids_documents_ids)
            .add(&new_users_ids_documents_ids)
            .r#union();

        // We stream and merge the new users ids documents ids map with the existing one.
        let mut users_ids_documents_ids_builder = fst::MapBuilder::memory();
        let mut iter = union_.into_stream();
        while let Some((user_id, vals)) = iter.next() {
            assert_eq!(vals.len(), 1, "there must be exactly one document id");
            users_ids_documents_ids_builder.insert(user_id, vals[0].value)?;
        }

        Ok(TransformOutput {
            primary_key: primary_key_field_id,
            fields_ids_map,
            users_ids_documents_ids: users_ids_documents_ids_builder.into_map(),
            new_documents_ids,
            replaced_documents_ids,
            documents_count,
            documents_file,
        })
    }
}
