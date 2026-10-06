//! Bounded format adapters followed by one DuckDB query path. No extensions are
//! downloaded at runtime, and no SQL from the previewed file is executed.
use calamine::{Reader, open_workbook_auto};
use duckdb::Connection;
use std::{
    io::Cursor,
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

pub const PAGE_SIZE: usize = 200;
pub const ROW_LIMIT: usize = 100_000;
pub const COLUMN_LIMIT: usize = 64;

pub type SharedTable = Arc<Mutex<TablePreview>>;

pub struct TablePreview {
    connection: Connection,
    pub sources: Vec<String>,
    pub sampled: bool,
    _directory: TempDir,
}

#[derive(Clone, Debug)]
pub struct TablePage {
    pub columns: Vec<(String, String)>,
    pub rows: Vec<Vec<String>>,
    pub offset: usize,
    pub more: bool,
    pub sampled: bool,
}

fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
fn identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
fn error(error: impl std::fmt::Display) -> String {
    format!("无法读取表格：{error}")
}

struct QueryTimeout(std::sync::mpsc::Sender<()>);
impl QueryTimeout {
    fn new(connection: &Connection) -> Self {
        let interrupt = connection.interrupt_handle();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if matches!(
                receiver.recv_timeout(std::time::Duration::from_secs(15)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                interrupt.interrupt();
            }
        });
        Self(sender)
    }
}
impl Drop for QueryTimeout {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

impl TablePreview {
    pub fn open(name: &str, bytes: &[u8]) -> Result<SharedTable, String> {
        if bytes.len() as u64 > super::FILE_LIMIT {
            return Err("文件超过 64 MiB，暂不预览".into());
        }
        let directory = tempfile::tempdir().map_err(error)?;
        let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
        let path = directory.path().join(format!("source.{ext}"));
        std::fs::write(&path, bytes).map_err(error)?;
        let connection = Connection::open_in_memory().map_err(error)?;
        let _timeout = QueryTimeout::new(&connection);
        connection.execute_batch(&format!("SET threads=2; SET memory_limit='128MB'; SET temp_directory={}; SET max_temp_directory_size='256MB'; SET autoinstall_known_extensions=false; SET autoload_known_extensions=false; LOAD json; LOAD parquet;", quoted(&directory.path().to_string_lossy()))).map_err(error)?;
        let mut this = Self {
            connection,
            sources: Vec::new(),
            sampled: false,
            _directory: directory,
        };
        match ext.as_str() {
            "xlsx" | "xls" | "xlsb" | "ods" => {
                if bytes.starts_with(b"PK") {
                    let zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(error)?;
                    let expanded = zip.decompressed_size().unwrap_or(u128::MAX);
                    if expanded > super::FILE_LIMIT as u128 {
                        return Err("工作簿解压内容超过 64 MiB".into());
                    }
                }
                let mut book = open_workbook_auto(&path).map_err(error)?;
                this.sampled |= book.sheet_names().len() > 32;
                for name in book.sheet_names().into_iter().take(32) {
                    let range = book.worksheet_range(&name).map_err(error)?;
                    if range.is_empty() {
                        continue;
                    }
                    this.sampled |= range.height() > ROW_LIMIT + 1 || range.width() > COLUMN_LIMIT;
                    let mut csv = csv::Writer::from_writer(Vec::new());
                    for row in range.rows().take(ROW_LIMIT + 1) {
                        csv.write_record(row.iter().take(COLUMN_LIMIT).map(ToString::to_string))
                            .map_err(error)?;
                    }
                    this.import_csv(&name, csv.into_inner().map_err(error)?, true)?;
                }
            }
            "sqlite" | "sqlite3" | "db" if bytes.starts_with(b"SQLite format 3\0") => {
                let db = rusqlite::Connection::open_with_flags(
                    &path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .map_err(error)?;
                db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")
                    .map_err(error)?;
                let mut names = db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' AND sql NOT LIKE '%VIRTUAL TABLE%' ORDER BY name LIMIT 33").map_err(error)?;
                let names = names
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(error)?;
                this.sampled |= names.len() > 32;
                for name in names.into_iter().take(32) {
                    let mut query = db
                        .prepare(&format!(
                            "SELECT * FROM {} LIMIT {}",
                            identifier(&name),
                            ROW_LIMIT + 1
                        ))
                        .map_err(error)?;
                    let count = query.column_count().min(COLUMN_LIMIT);
                    this.sampled |= query.column_count() > COLUMN_LIMIT;
                    let mut csv = csv::Writer::from_writer(Vec::new());
                    csv.write_record(query.column_names().into_iter().take(count))
                        .map_err(error)?;
                    let mut rows = query.query([]).map_err(error)?;
                    let mut ix = 0;
                    while let Some(row) = rows.next().map_err(error)? {
                        if ix == ROW_LIMIT {
                            this.sampled = true;
                            break;
                        }
                        let values = (0..count)
                            .map(|col| -> Result<String, String> {
                                use rusqlite::types::ValueRef;
                                Ok(match row.get_ref(col).map_err(error)? {
                                    ValueRef::Null => String::new(),
                                    ValueRef::Integer(value) => value.to_string(),
                                    ValueRef::Real(value) => value.to_string(),
                                    ValueRef::Text(value) => {
                                        String::from_utf8_lossy(&value[..value.len().min(4096)])
                                            .into_owned()
                                    }
                                    ValueRef::Blob(value) => {
                                        format!("[BLOB: {} bytes]", value.len())
                                    }
                                })
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        csv.write_record(values).map_err(error)?;
                        ix += 1;
                    }
                    this.import_csv(&name, csv.into_inner().map_err(error)?, true)?;
                }
            }
            "duckdb" | "ddb" | "db" => {
                this.connection
                    .execute_batch(&format!(
                        "ATTACH {} AS source (READ_ONLY);",
                        quoted(&path.to_string_lossy())
                    ))
                    .map_err(error)?;
                let mut query = this.connection.prepare("SELECT table_schema, table_name FROM information_schema.tables WHERE table_catalog='source' AND table_type='BASE TABLE' ORDER BY table_schema, table_name LIMIT 33").map_err(error)?;
                let names = query
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(error)?;
                drop(query);
                this.sampled |= names.len() > 32;
                for (schema, name) in names.into_iter().take(32) {
                    this.import_select(
                        &format!("{schema}.{name}"),
                        &format!(
                            "SELECT * FROM source.{}.{}",
                            identifier(&schema),
                            identifier(&name)
                        ),
                    )?;
                }
                this.connection
                    .execute_batch("DETACH source")
                    .map_err(error)?;
            }
            "avro" => {
                apache_avro::util::max_allocation_bytes(super::FILE_LIMIT as usize);
                let mut json = Vec::new();
                let reader = apache_avro::Reader::new(Cursor::new(bytes)).map_err(error)?;
                for (ix, row) in reader.enumerate() {
                    if ix == ROW_LIMIT {
                        this.sampled = true;
                        break;
                    }
                    let row = row.map_err(error)?;
                    serde_json::to_writer(&mut json, &avro_json(row)).map_err(error)?;
                    json.push(b'\n');
                    if json.len() as u64 > super::FILE_LIMIT {
                        this.sampled = true;
                        break;
                    }
                }
                let json_path = this._directory.path().join("converted.jsonl");
                std::fs::write(&json_path, json).map_err(error)?;
                this.import_select(
                    name,
                    &format!(
                        "SELECT * FROM read_json_auto({}, format='newline_delimited')",
                        quoted(&json_path.to_string_lossy())
                    ),
                )?;
            }
            "arrow" | "arrows" | "feather" => {
                let mut csv = csv::Writer::from_writer(Vec::new());
                let mut seen = 0;
                let mut header = false;
                let mut write =
                    |batch: Result<_, duckdb::arrow::error::ArrowError>| -> Result<bool, String> {
                        let batch: duckdb::arrow::record_batch::RecordBatch =
                            batch.map_err(error)?;
                        let count = batch.num_rows().min(ROW_LIMIT - seen);
                        if count < batch.num_rows() {
                            this.sampled = true;
                        }
                        if batch.num_columns() > COLUMN_LIMIT {
                            this.sampled = true;
                        }
                        let columns = batch.num_columns().min(COLUMN_LIMIT);
                        if !header {
                            csv.write_record(
                                batch
                                    .schema()
                                    .fields()
                                    .iter()
                                    .take(columns)
                                    .map(|field| field.name()),
                            )
                            .map_err(error)?;
                            header = true;
                        }
                        for row in 0..count {
                            let values = batch
                                .columns()
                                .iter()
                                .take(columns)
                                .map(|column| {
                                    if column.is_null(row) {
                                        Ok(String::new())
                                    } else {
                                        // The standard Arrow CSV writer rejects lists and structs.
                                        // Display nested values as cells alongside scalar fields.
                                        duckdb::arrow::util::display::array_value_to_string(
                                            column.as_ref(),
                                            row,
                                        )
                                        .map_err(error)
                                    }
                                })
                                .collect::<Result<Vec<_>, String>>()?;
                            csv.write_record(values).map_err(error)?;
                            if csv.get_ref().len() as u64 > super::FILE_LIMIT {
                                this.sampled = true;
                                return Ok(false);
                            }
                        }
                        seen += count;
                        Ok(seen < ROW_LIMIT)
                    };
                if bytes.starts_with(b"ARROW1") {
                    for batch in arrow_ipc::reader::FileReader::try_new(Cursor::new(bytes), None)
                        .map_err(error)?
                    {
                        if !write(batch)? {
                            this.sampled = true;
                            break;
                        }
                    }
                } else {
                    for batch in arrow_ipc::reader::StreamReader::try_new(Cursor::new(bytes), None)
                        .map_err(error)?
                    {
                        if !write(batch)? {
                            this.sampled = true;
                            break;
                        }
                    }
                }
                this.import_csv(name, csv.into_inner().map_err(error)?, true)?;
            }
            "sqlite" | "sqlite3" => return Err("文件头不是 SQLite 数据库".into()),
            _ => {
                let source = quoted(&path.to_string_lossy());
                let query = match ext.as_str() {
                    "parquet" | "parq" => format!("SELECT * FROM read_parquet({source})"),
                    "json" | "jsonl" | "ndjson" => format!(
                        "SELECT * FROM read_json_auto({source}, maximum_depth=32, maximum_object_size=16777216)"
                    ),
                    "tsv" => format!(
                        "SELECT * FROM read_csv({source}, delim='\t', header=true, auto_detect=true)"
                    ),
                    _ => format!("SELECT * FROM read_csv_auto({source})"),
                };
                this.import_select(name, &query)?;
            }
        }
        if this.sources.is_empty() {
            return Err("没有可预览的数据表或工作表".into());
        }
        this.connection
            .execute_batch("SET enable_external_access=false")
            .map_err(error)?;
        Ok(Arc::new(Mutex::new(this)))
    }

    fn import_csv(&mut self, name: &str, bytes: Vec<u8>, header: bool) -> Result<(), String> {
        let path = self
            ._directory
            .path()
            .join(format!("adapter-{}.csv", self.sources.len()));
        std::fs::write(&path, bytes).map_err(error)?;
        self.import_select(
            name,
            &format!(
                "SELECT * FROM read_csv_auto({}, header={header}, nullstr='')",
                quoted(&path.to_string_lossy())
            ),
        )
    }

    fn import_select(&mut self, name: &str, select: &str) -> Result<(), String> {
        let index = self.sources.len();
        // DESCRIBE does not execute stored views or arbitrary user SQL.
        let mut describe = self
            .connection
            .prepare(&format!("DESCRIBE ({select})"))
            .map_err(error)?;
        let columns = describe
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(error)?;
        drop(describe);
        self.sampled |= columns.len() > COLUMN_LIMIT;
        let columns = columns
            .iter()
            .take(COLUMN_LIMIT)
            .map(|c| identifier(c))
            .collect::<Vec<_>>()
            .join(", ");
        self.connection
            .execute_batch(&format!(
                "CREATE TABLE preview_{index} AS SELECT {columns} FROM ({select}) LIMIT {}",
                ROW_LIMIT + 1
            ))
            .map_err(error)?;
        let count: usize = self
            .connection
            .query_row(
                &format!("SELECT count(*) FROM preview_{index}"),
                [],
                |row| row.get(0),
            )
            .map_err(error)?;
        self.sampled |= count > ROW_LIMIT;
        self.sources.push(name.to_owned());
        Ok(())
    }

    pub fn page(
        &self,
        source: usize,
        offset: usize,
        sort: Option<(usize, bool)>,
    ) -> Result<TablePage, String> {
        let _timeout = QueryTimeout::new(&self.connection);
        if source >= self.sources.len() {
            return Err("数据表不存在".into());
        }
        let table = format!("preview_{source}");
        let mut describe = self
            .connection
            .prepare(&format!("DESCRIBE {table}"))
            .map_err(error)?;
        let columns = describe
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(error)?;
        drop(describe);
        let projection = columns
            .iter()
            .map(|(name, _)| format!("substr(CAST({} AS VARCHAR), 1, 4096)", identifier(name)))
            .collect::<Vec<_>>()
            .join(", ");
        let order = sort
            .filter(|(col, _)| *col < columns.len())
            .map(|(col, ascending)| {
                format!(
                    " ORDER BY {} {} NULLS LAST",
                    identifier(&columns[col].0),
                    if ascending { "ASC" } else { "DESC" }
                )
            })
            .unwrap_or_default();
        let offset = offset.min(ROW_LIMIT);
        let mut query = self
            .connection
            .prepare(&format!(
                "SELECT {projection} FROM {table}{order} LIMIT {} OFFSET {offset}",
                PAGE_SIZE + 1
            ))
            .map_err(error)?;
        let rows = query
            .query_map([], |row| {
                (0..columns.len())
                    .map(|col| {
                        row.get::<_, Option<String>>(col)
                            .map(|v| v.unwrap_or_else(|| "NULL".into()))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .map_err(error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(error)?;
        let more = rows.len() > PAGE_SIZE && offset + PAGE_SIZE < ROW_LIMIT;
        Ok(TablePage {
            columns,
            rows: rows.into_iter().take(PAGE_SIZE).collect(),
            offset,
            more,
            sampled: self.sampled,
        })
    }
}

fn avro_json(value: apache_avro::types::Value) -> serde_json::Value {
    use apache_avro::types::Value as A;
    match value {
        A::Null => serde_json::Value::Null,
        A::Boolean(v) => v.into(),
        A::Int(v) => v.into(),
        A::Long(v) => v.into(),
        A::Float(v) => serde_json::json!(v),
        A::Double(v) => serde_json::json!(v),
        A::String(v) | A::Enum(_, v) => v.into(),
        A::Union(_, v) => avro_json(*v),
        A::Array(v) => serde_json::Value::Array(v.into_iter().map(avro_json).collect()),
        A::Map(v) => {
            serde_json::Value::Object(v.into_iter().map(|(k, v)| (k, avro_json(v))).collect())
        }
        A::Record(v) => {
            serde_json::Value::Object(v.into_iter().map(|(k, v)| (k, avro_json(v))).collect())
        }
        A::Bytes(v) | A::Fixed(_, v) => serde_json::json!(format!("[BLOB: {} bytes]", v.len())),
        v => serde_json::json!(format!("{v:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_parts(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (path, text) in parts {
            zip.start_file(*path, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }
    fn first_row(name: &str, bytes: &[u8]) -> Vec<String> {
        let session = TablePreview::open(name, bytes).unwrap();
        session.lock().unwrap().page(0, 0, None).unwrap().rows[0].clone()
    }
    #[test]
    fn json_records_and_tsv_are_tables() {
        assert_eq!(
            first_row("test.json", br#"[{"id":7,"title":"hello"}]"#),
            ["7", "hello"]
        );
        assert_eq!(
            first_row("test.ndjson", b"{\"id\":7,\"title\":\"hello\"}\n"),
            ["7", "hello"]
        );
        assert_eq!(
            first_row("test.tsv", b"id\ttitle\n7\thello\n"),
            ["7", "hello"]
        );
    }
    #[test]
    fn native_duckdb_and_parquet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.duckdb");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE test AS SELECT 7::BIGINT AS id, 'hello' AS title; CHECKPOINT;",
        )
        .unwrap();
        let parquet = dir.path().join("test.parquet");
        db.execute_batch(&format!(
            "COPY test TO {} (FORMAT PARQUET)",
            quoted(&parquet.to_string_lossy())
        ))
        .unwrap();
        drop(db);
        assert_eq!(
            first_row("test.duckdb", &std::fs::read(path).unwrap()),
            ["7", "hello"]
        );
        assert_eq!(
            first_row("test.parquet", &std::fs::read(parquet).unwrap()),
            ["7", "hello"]
        );
    }
    #[test]
    fn avro_and_both_arrow_container_formats() {
        use apache_avro::{Schema, Writer, types::Value};
        let schema=Schema::parse_str(r#"{"type":"record","name":"test","fields":[{"name":"id","type":"long"},{"name":"title","type":"string"}]}"#).unwrap();
        let mut writer = Writer::new(&schema, Vec::new());
        writer
            .append(Value::Record(vec![
                ("id".into(), Value::Long(7)),
                ("title".into(), Value::String("hello".into())),
            ]))
            .unwrap();
        assert_eq!(
            first_row("test.avro", &writer.into_inner().unwrap()),
            ["7", "hello"]
        );
        use duckdb::arrow::{
            array::{Int64Array, StringArray},
            datatypes::{DataType, Field, Schema as ArrowSchema},
            record_batch::RecordBatch,
        };
        let schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("title", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![7])),
                Arc::new(StringArray::from(vec!["hello"])),
            ],
        )
        .unwrap();
        let mut bytes = Vec::new();
        {
            let mut writer = arrow_ipc::writer::FileWriter::try_new(&mut bytes, &schema).unwrap();
            writer.write(&batch).unwrap();
            writer.finish().unwrap();
        }
        assert_eq!(first_row("test.feather", &bytes), ["7", "hello"]);
        bytes.clear();
        {
            let mut writer = arrow_ipc::writer::StreamWriter::try_new(&mut bytes, &schema).unwrap();
            writer.write(&batch).unwrap();
            writer.finish().unwrap();
        }
        assert_eq!(first_row("test.arrows", &bytes), ["7", "hello"]);
    }
    #[test]
    fn arrow_nested_values_are_cells() {
        use duckdb::arrow::{
            array::{Array, ListArray},
            datatypes::{Field, Int32Type, Schema},
            record_batch::RecordBatch,
        };
        let array =
            ListArray::from_iter_primitive::<Int32Type, _, _>([Some(vec![Some(1), Some(2)])]);
        let schema = Arc::new(Schema::new(vec![Field::new(
            "items",
            array.data_type().clone(),
            true,
        )]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(array)]).unwrap();
        let mut bytes = Vec::new();
        let mut writer = arrow_ipc::writer::FileWriter::try_new(&mut bytes, &schema).unwrap();
        writer.write(&batch).unwrap();
        writer.finish().unwrap();
        drop(writer);
        assert_eq!(first_row("nested.arrow", &bytes), ["[1, 2]"]);
    }
    #[test]
    fn xlsx_and_ods_adapters() {
        let xlsx = zip_parts(&[
            (
                "_rels/.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "[Content_Types].xml",
                r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:B2"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>id</t></is></c><c r="B1" t="inlineStr"><is><t>title</t></is></c></row><row r="2"><c r="A2"><v>7</v></c><c r="B2" t="inlineStr"><is><t>hello</t></is></c></row></sheetData></worksheet>"#,
            ),
        ]);
        assert_eq!(first_row("test.xlsx", &xlsx), ["7", "hello"]);
        let ods = zip_parts(&[
            ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
            (
                "META-INF/manifest.xml",
                r#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>"#,
            ),
            (
                "content.xml",
                r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Sheet1"><table:table-row><table:table-cell office:value-type="string"><text:p>id</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>title</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="float" office:value="7"/><table:table-cell office:value-type="string"><text:p>hello</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#,
            ),
        ]);
        assert_eq!(first_row("test.ods", &ods), ["7", "hello"]);
    }
    #[test]
    fn csv_pages_and_typed_sort() {
        let bytes = format!(
            "id,name\n{}",
            (0..450)
                .rev()
                .map(|i| format!("{i},row{i}\n"))
                .collect::<String>()
        );
        let table = TablePreview::open("sample.csv", bytes.as_bytes()).unwrap();
        let table = table.lock().unwrap();
        let page = table.page(0, 0, Some((0, true))).unwrap();
        assert_eq!(page.rows.len(), 200);
        assert_eq!(page.rows[0][0], "0");
        assert!(page.more);
        assert_eq!(page.columns[0].1, "BIGINT");
        let page = table.page(0, 400, Some((0, true))).unwrap();
        assert_eq!(page.rows.len(), 50);
        assert!(!page.more);
    }
    #[test]
    fn sqlite_reads_tables_without_executing_views() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE test(id INTEGER, title TEXT); INSERT INTO test VALUES(7,'hello'); CREATE VIEW skipped AS SELECT * FROM test;").unwrap();
        drop(db);
        let table = TablePreview::open("test.db", &std::fs::read(path).unwrap()).unwrap();
        let table = table.lock().unwrap();
        assert_eq!(table.sources, ["test"]);
        assert_eq!(table.page(0, 0, None).unwrap().rows, [vec!["7", "hello"]]);
        assert!(TablePreview::open("fake.sqlite", b"hello").is_err());
    }
}
