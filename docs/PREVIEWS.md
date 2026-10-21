# File previews

Select a file and press **Space**. Parsing and decoding run in background workers
for local and remote connections alike. Moving the selection discards stale
results; no preview edits the original file.

| Files | Preview |
| --- | --- |
| CSV, TSV | DuckDB data grid with inferred field names and types |
| Parquet (`parquet`, `parq`) | DuckDB data grid |
| Excel (`xlsx`, `xls`, `xlsb`), ODS | Worksheet selection and DuckDB data grid |
| JSON record arrays, JSONL, NDJSON | DuckDB data grid |
| SQLite (`sqlite`, `sqlite3`, `db`) | Table selection and DuckDB data grid; SQLite magic bytes are verified |
| DuckDB (`duckdb`, `ddb`, `db`) | Table selection and DuckDB data grid |
| Avro | Record adapter and DuckDB data grid |
| Arrow IPC files/streams (`arrow`, `arrows`), Feather V2 | Arrow adapter and DuckDB data grid |
| JSON objects, XML, YAML, TOML | Collapsible tree and formatted code view |
| PDF | Page navigation and zoom; encrypted files must be decrypted first |
| PNG, JPEG, GIF, WebP, BMP, SVG, ICO | Image preview |
| TIFF (`tif`, `tiff`) | Page navigation for multi-page images |
| Directories, ZIP, TAR, TAR.GZ, TGZ, TAR.BZ2, TAR.XZ, 7Z | Directory tree, without extracting files |
| DOCX, ODT, RTF | Content preview; Office headings, paragraphs, tables and embedded images |
| PPTX | Slide selection, page image and readable text; complex themes, masters and animation are simplified |
| MP3, WAV, FLAC, OGG, M4A | Play/pause, replay, seek ±10 seconds, position, duration and audio metadata |
| MP4, MOV, WEBM, MKV | First frame, duration, resolution and codecs |
| Markdown, text, source code | Existing rendered Markdown or plain text preview |

The data grid shows **200 rows per page**, supports horizontal scrolling and
typed column sorting, and switches between sheets or tables. A preview samples
at most **100,000 rows, 64 columns and 32 sheets/tables**; a notice identifies
sampled results. Individual displayed cell values are limited to 4,096
characters. Database views and virtual SQLite tables are excluded.

DuckDB is embedded. Excel, SQLite, Avro and Arrow use Rust format adapters before
importing their data into DuckDB, so previewing these formats does not install
or download DuckDB extensions. Excel/SQLite/Arrow adapters convert values to CSV;
types are inferred from those values, so a source's declared types and null vs.
empty-string distinctions may be simplified. Avro logical types outside basic
record values are displayed as descriptions. Arrow lists and structs are shown
as text inside cells.

The first build compiles bundled DuckDB C++ sources and takes longer. XZ support
is statically linked, so installers do not require a system/Homebrew liblzma.

Video thumbnails require **`ffmpeg` and `ffprobe`** on PATH. On macOS, Homebrew's
standard locations are also detected. Audio playback and all other previews
work without FFmpeg. Video playback is not currently embedded.

## Bounds

- Text: first 128 KiB. Images: at most 8 MiB.
- ZIP/ZIP64: no archive-size limit. Read object metadata, about 64 KiB from the
  end, then at most 8 MiB from the central directory. Small directories already
  in the tail need no second read. Display at most 2,000 central-directory
  entries; the preview indicates truncation. File bodies are not decompressed
  or downloaded in full. Conditional reads use a strong ETag when the backend
  supports it; changes cause an error instead of mixing object revisions.
  Split archives and encrypted/compressed central directories are unsupported.
- Other whole-file previews: at most 64 MiB, including files of unknown size.
- Trees: at most 2,000 entries/nodes. Compressed TAR traversal inflates at most
  128 MiB. Archive contents are never written to disk.
- PDF/TIFF/PPTX: first 500 pages. Selected PDF pages and slide images render on
  demand. TIFF/raster images larger than 16 million pixels are refused.
- Office ZIP parts: at most 8 MiB per part. Workbook expanded size: at most
  64 MiB. Document text: first 128 KiB; embedded images: at most 32 / 16 MiB.
- DuckDB: two worker threads, 128 MiB memory, 256 MiB temporary spill space and
  a 15-second query interrupt. Video helpers stop after 20 seconds.

Private temporary directories hold staged table and video files and are removed
when the preview is released. SVG external image references and XML DTDs are
not loaded. Unsupported, encrypted and damaged files show a readable error.

Run the native preview harness with any supported file to inspect actual
platform text/image rendering:

```sh
cargo run -p roam-ui --example preview_panel -- /absolute/path/to/file
```

To verify S3 ZIP range requests locally without cloud credentials (Postman CLI
1.70+), start the committed HTTP simulation, then run the integration test in
another terminal:

```sh
postman simulation run postman/simulations/s3-zip.sim.yaml --port 4900 --no-history --output ndjson
ROAM_ZIP_S3_ENDPOINT=http://127.0.0.1:4900/s3 cargo test -p roam-core --test zip_ranges -- --nocapture
```

The fixtures model 64 MiB ZIP and 5 GiB ZIP64 objects without allocating their
payloads. The server rejects missing ranges, payload reads and missing ETag
conditions. The test also covers replacement (HTTP 412) and short responses.
This verifies the real OpenDAL S3 HTTP path against the local model; it does not
validate credentials, signatures or permissions against a live S3 account.
