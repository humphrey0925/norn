// Just enough of the zip format to read a .vsix: the names of what is in
// it, and the bytes of one of them (stored or deflated).
import * as fs from "fs";
import * as path from "path";
import * as zlib from "zlib";

export interface Entry {
  name: string;
  method: number;
  size: number;
  at: number;
}

export function entries(bytes: Buffer): Entry[] {
  // The end of the central directory: its signature, searched for from the end.
  let end = bytes.length - 22;
  while (end >= 0 && bytes.readUInt32LE(end) !== 0x06054b50) {
    end--;
  }
  if (end < 0) {
    throw new Error("not a zip file");
  }
  const count = bytes.readUInt16LE(end + 10);
  let at = bytes.readUInt32LE(end + 16);
  const out: Entry[] = [];
  for (let i = 0; i < count; i++) {
    const nameLength = bytes.readUInt16LE(at + 28);
    const extra = bytes.readUInt16LE(at + 30);
    const comment = bytes.readUInt16LE(at + 32);
    out.push({
      name: bytes.toString("utf8", at + 46, at + 46 + nameLength),
      method: bytes.readUInt16LE(at + 10),
      size: bytes.readUInt32LE(at + 20),
      at: bytes.readUInt32LE(at + 42),
    });
    at += 46 + nameLength + extra + comment;
  }
  return out;
}

export function read(bytes: Buffer, e: Entry): Buffer {
  // The local header repeats the name and has an extra field of its own.
  const data = e.at + 30 + bytes.readUInt16LE(e.at + 26) + bytes.readUInt16LE(e.at + 28);
  const raw = bytes.subarray(data, data + e.size);
  return e.method === 0 ? Buffer.from(raw) : zlib.inflateRawSync(raw);
}

/** Write every file of the zip under `dir`. */
export function extract(file: string, dir: string): void {
  const bytes = fs.readFileSync(file);
  for (const e of entries(bytes)) {
    if (e.name.endsWith("/")) {
      continue;
    }
    const to = path.join(dir, e.name);
    fs.mkdirSync(path.dirname(to), { recursive: true });
    fs.writeFileSync(to, read(bytes, e));
  }
}
