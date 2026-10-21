// Local model of AWS HeadObject/GetObject Range + If-Match, following:
// https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html
// No live credentials or objects. Owner mock discovery was skipped because
// this CLI session is signed out. Unsupported operations return 501.
const http = require('http');
const etag = '"roam-zip-fixture"';
const entries = 2001;
const records = [];
for (let i = 0; i < entries; i++) {
  const name = Buffer.from(`docs/file-${String(i).padStart(4, '0')}.txt`);
  const header = Buffer.alloc(46);
  header.writeUInt32LE(0x02014b50, 0);
  header.writeUInt16LE(20, 4);
  header.writeUInt16LE(20, 6);
  header.writeUInt16LE(name.length, 28);
  records.push(header, name);
}
const directory = Buffer.concat(records);
function archive(zip64, extraSize = 0) {
  const offset = zip64 ? 5 * 1024 ** 3 : 64 * 1024 ** 2;
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  let trailer;
  if (zip64) {
    const end = Buffer.alloc(56 + extraSize);
    end.writeUInt32LE(0x06064b50, 0);
    end.writeBigUInt64LE(BigInt(44 + extraSize), 4);
    end.writeUInt16LE(45, 12);
    end.writeUInt16LE(45, 14);
    end.writeBigUInt64LE(BigInt(entries), 24);
    end.writeBigUInt64LE(BigInt(entries), 32);
    end.writeBigUInt64LE(BigInt(directory.length), 40);
    end.writeBigUInt64LE(BigInt(offset), 48);
    const locator = Buffer.alloc(20);
    locator.writeUInt32LE(0x07064b50, 0);
    locator.writeBigUInt64LE(BigInt(offset + directory.length), 8);
    locator.writeUInt32LE(1, 16);
    eocd.writeUInt16LE(0xffff, 8);
    eocd.writeUInt16LE(0xffff, 10);
    eocd.writeUInt32LE(0xffffffff, 12);
    eocd.writeUInt32LE(0xffffffff, 16);
    trailer = Buffer.concat([end, locator, eocd]);
  } else {
    eocd.writeUInt16LE(entries, 8);
    eocd.writeUInt16LE(entries, 10);
    eocd.writeUInt32LE(directory.length, 12);
    eocd.writeUInt32LE(offset, 16);
    trailer = eocd;
  }
  return {offset, metadata: Buffer.concat([directory, trailer])};
}
const classic = archive(false);
const zip64 = archive(true);
const extendedZip64 = archive(true, 70000);
http.createServer((req, res) => {
  const url = new URL(req.url, 'http://local');
  const key = url.pathname.split('/').pop();
  if (!['large.zip', 'zip64.zip', 'zip64-extra.zip', 'changed.zip', 'short.zip'].includes(key)) {
    res.writeHead(501).end(); return;
  }
  const file = key === 'zip64-extra.zip' ? extendedZip64 : key === 'zip64.zip' ? zip64 : classic;
  const size = file.offset + file.metadata.length;
  const headers = {'Content-Length': size, ETag: etag, 'Accept-Ranges': 'bytes', 'Content-Type': 'application/zip'};
  if (req.method === 'HEAD') { res.writeHead(200, headers).end(); return; }
  if (req.method !== 'GET') { res.writeHead(501).end(); return; }
  if (req.headers['if-match'] !== etag || key === 'changed.zip') {
    res.writeHead(412, {'Content-Type': 'application/xml'}).end('<Error><Code>PreconditionFailed</Code></Error>'); return;
  }
  const range = /^bytes=(\d+)-(\d+)$/.exec(req.headers.range || '');
  if (!range) { res.writeHead(400).end('Expected one bounded Range'); return; }
  const start = Number(range[1]), end = Number(range[2]);
  if (start > end || end >= size) { res.writeHead(416).end(); return; }
  // A successful test must never request payload/local-header bytes.
  if (start < file.offset || end - start + 1 > 8 * 1024 ** 2) {
    res.writeHead(400).end('Preview requested payload or exceeded metadata budget'); return;
  }
  let body = file.metadata.subarray(start - file.offset, end + 1 - file.offset);
  if (key === 'short.zip') body = body.subarray(0, body.length - 1);
  res.writeHead(206, {...headers, 'Content-Length': body.length, 'Content-Range': `bytes ${start}-${end}/${size}`}).end(body);
}).listen(process.env.PORT);
