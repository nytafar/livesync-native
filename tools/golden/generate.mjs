// Emits golden test vectors from @vrtmrz/livesync-commonlib (pinned in package.json)
// for the Rust crates. Usage: node generate.mjs <out-dir>
//
// Fixtures:
//   path2id.json      path -> document ID (path2id_base, no obfuscation)
//   chunk_ids.json    piece -> chunk ID (HashManager, hashAlg=xxhash64, encryption off)
//   decompress.json   compressed `data` field -> decompressed `data` (decompressDoc)
//   ignore.json       path -> shouldBeIgnored / isPlainText
import { mkdirSync, writeFileSync, readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";

import { path2id_base, shouldBeIgnored, isPlainText } from "@vrtmrz/livesync-commonlib/compat/string_and_binary/path";
import { HashManager } from "@vrtmrz/livesync-commonlib/hashing";

// compress.js is not in the package's export map; import it by file URL.
const here = dirname(fileURLToPath(import.meta.url));
const pkgDir = join(here, "node_modules", "@vrtmrz", "livesync-commonlib");
const pkg = JSON.parse(readFileSync(join(pkgDir, "package.json"), "utf8"));
const { compressDoc, decompressDoc } = await import(pathToFileURL(join(pkgDir, "dist", "pouchdb", "compress.js")).href);

const outDir = process.argv[2] ?? join(here, "..", "..", "fixtures", "golden");
mkdirSync(outDir, { recursive: true });
const meta = { generator: "tools/golden/generate.mjs", package: pkg.name, version: pkg.version };
const emit = (name, cases) => {
    writeFileSync(join(outDir, name), JSON.stringify({ meta, cases }, null, 1) + "\n");
    console.log(`${name}: ${cases.length} cases`);
};

// Deterministic pseudo-random bytes (xorshift32) so fixtures are stable.
let seed = 0x9e3779b9;
const rnd = () => {
    seed ^= seed << 13;
    seed >>>= 0;
    seed ^= seed >>> 17;
    seed ^= seed << 5;
    seed >>>= 0;
    return seed;
};
const randomBytes = (n) => Uint8Array.from({ length: n }, () => rnd() & 0xff);
const b64 = (u8) => Buffer.from(u8).toString("base64");

// ---------- path -> id ----------
const paths = [
    "note.md",
    "Folder/Sub Folder/Note With Spaces.md",
    "README.MD",
    "_underscore.md",
    "_Folder/_inner.md",
    "folder/_inner.md",
    "Ærlig talt – ÆØÅ æøå.md",
    "Straße/Größe.md",
    "İstanbul.md",
    "ΣΊΣΥΦΟΣ.md",
    "Café.md", // NFC
    "Café.md", // NFD
    "emoji 🎉/😀 face.md",
    "日本語/ノート.md",
    "attachments/Image 1.PNG",
    ".obsidian/app.json",
    "i:.obsidian/app.json",
    "ix:Term/category/Name.md",
    "ps:obsolete.md",
    "a/b/c/d/e.canvas",
    " leading space.md",
    "trailing space .md",
    "ǅ titlecase.md",
    "K kelvin sign.md",
    "ﬃ ligature.md",
];
const pathCases = [];
for (const path of paths) {
    for (const caseInsensitive of [true, false]) {
        pathCases.push({ path, caseInsensitive, id: await path2id_base(path, false, caseInsensitive) });
    }
}
emit("path2id.json", pathCases);

// ---------- chunk ids ----------
const settings = {
    hashAlg: "xxhash64",
    encrypt: false,
    passphrase: "",
    usePathObfuscation: false,
    idDerivationVersion: 0,
    idDerivationKey: "",
};
const hm = new HashManager({ settingService: { currentSettings: () => settings } });
await hm.initialise();
const pieces = [
    "",
    "a",
    "hello world",
    "# Heading\n\nSome text with a [[link]].\n",
    "æøå ÆØÅ",
    "😀🎉 surrogate pairs",
    "日本語のテキスト",
    "line\r\nwith crlf\r\n",
    "x".repeat(1000),
    "multi-byte ".repeat(200) + "ü".repeat(300),
    b64(randomBytes(3)),
    b64(randomBytes(100)),
    b64(randomBytes(4096)),
    b64(randomBytes(100000)),
];
const chunkCases = [];
for (const piece of pieces) chunkCases.push({ piece, id: "h:" + (await hm.computeHash(piece)) });
emit("chunk_ids.json", chunkCases);

// ---------- decompression ----------
const decompressInputs = [
    { kind: "text", data: "Hello, compressed world!\n".repeat(40) },
    { kind: "text", data: "æøå ÆØÅ 日本語 😀\n".repeat(50) },
    { kind: "text", data: "short" },
    { kind: "base64", data: b64(new Uint8Array(4096)) },
    { kind: "base64", data: b64(randomBytes(2048)) },
    { kind: "base64", data: b64(Uint8Array.from({ length: 5000 }, (_, i) => i % 7)) },
];
const decompressCases = [];
for (const { kind, data } of decompressInputs) {
    // Force compression regardless of size gain so every case carries the marker.
    const { _compressText, MARK_SHIFT_COMPRESSED } = await import(
        pathToFileURL(join(pkgDir, "dist", "pouchdb", "compress.js")).href
    );
    const compressed = MARK_SHIFT_COMPRESSED + (await _compressText(data));
    const roundTrip = await decompressDoc({ _id: "h:x", type: "leaf", data: compressed });
    if (roundTrip.data !== data) throw new Error(`round trip failed for ${kind}`);
    decompressCases.push({ kind, compressed, expected: data });
    // compressDoc only keeps compression when it is shorter; record that too.
    const viaDoc = await compressDoc({ _id: "h:x", type: "leaf", data });
    decompressCases.push({ kind: kind + "-compressDoc", compressed: viaDoc.data, expected: data });
}
// Uncompressed data passes through unchanged.
decompressCases.push({ kind: "plain-passthrough", compressed: "no marker here", expected: "no marker here" });
emit("decompress.json", decompressCases);

// ---------- ignore / plain-text classification ----------
const classify = [
    "redflag.md",
    "redflag2.md",
    "redflag3.md",
    "flag_rebuild.md",
    "flag_fetch.md",
    "livesync_log_2026.md",
    "LIVESYNC_LOG_x.md",
    "sub/redflag.md",
    "note.md",
    "a.txt",
    "a.svg",
    "a.html",
    "a.csv",
    "a.css",
    "a.js",
    "a.xml",
    "a.canvas",
    "a.json",
    "a.png",
    "a.MD",
    "a.pdf",
];
emit(
    "classify.json",
    classify.map((path) => ({ path, ignored: shouldBeIgnored(path), plainText: isPlainText(path) }))
);
