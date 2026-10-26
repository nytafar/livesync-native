// Emits golden test vectors from @vrtmrz/livesync-commonlib (pinned in package.json)
// for the Rust crates. Usage: node generate.mjs <out-dir>
//
// Fixtures:
//   path2id.json      path -> document ID (path2id_base, no obfuscation)
//   chunk_ids.json    piece -> chunk ID (HashManager, hashAlg=xxhash64, encryption off)
//   decompress.json   compressed `data` field -> decompressed `data` (decompressDoc)
//   classify.json     path -> shouldBeIgnored / isPlainText
//   splitter.json     v3-rabin-karp splits (ContentSplitter, as EntryManager uses it):
//                     inputs are recipes both sides regenerate, outputs are piece
//                     lengths (in UTF-8 bytes) and chunk IDs
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

// ---------- splitter (v3-rabin-karp) ----------
// Inputs are generated from recipes that crates/livesync-model/tests/golden.rs
// reproduces byte for byte (FNV-1a over the bytes guards against drift).
const { ContentSplitter } = await import(pathToFileURL(join(pkgDir, "dist", "ContentSplitter", "ContentSplitters.js")).href);
const { createTextBlob, createBinaryBlob, isPlainText: isPlainTextUtil } = await import(
    pathToFileURL(join(pkgDir, "dist", "common", "utils.js")).href
).then(async (u) => ({ ...u, isPlainText }));

const VOCAB = [
    "the ", "and ", "of ", "note ", "LiveSync ", " ", " ", "\n", "\n\n", "# Heading\n", "- [ ] task\n",
    "[[link]] ", "æøå ", "ÆØÅ ", "Straße ", "日本語の", "テキスト", "😀", "🎉 ", "ü", "€", "\t", "x", "é",
    "```js\nconst a = 1;\n```\n", "| a | b |\n", "> quote\n", "CRLF\r\n",
];
const xorshift = (s) => () => {
    s ^= s << 13;
    s >>>= 0;
    s ^= s >>> 17;
    s ^= s << 5;
    s >>>= 0;
    return s;
};
const enc = new TextEncoder();
function makeInput(recipe) {
    switch (recipe.kind) {
        case "literal":
            return { text: recipe.text };
        case "repeat":
            return { text: recipe.unit.repeat(recipe.count) };
        case "vocab": {
            const r = xorshift(recipe.seed);
            const parts = [];
            let len = 0;
            while (len < recipe.bytes) {
                const t = VOCAB[r() % VOCAB.length];
                parts.push(t);
                len += enc.encode(t).length;
            }
            return { text: parts.join("") };
        }
        case "random": {
            const r = xorshift(recipe.seed);
            return { bytes: Uint8Array.from({ length: recipe.bytes }, () => r() & 0xff) };
        }
        case "fill":
            return { bytes: new Uint8Array(recipe.bytes).fill(recipe.value) };
        default:
            throw new Error("unknown recipe " + recipe.kind);
    }
}
const fnv1a = (u8) => {
    let h = 0x811c9dc5;
    for (const b of u8) h = Math.imul(h ^ b, 0x01000193) >>> 0;
    return h >>> 0;
};
const MB = 1024 * 1024;
const splitCases = [
    // text: boundaries, multibyte, sizes across the unit growth and the 4 MB switch
    { path: "empty.md", recipe: { kind: "literal", text: "" } },
    { path: "one.md", recipe: { kind: "literal", text: "a" } },
    { path: "w47.md", recipe: { kind: "repeat", unit: "a", count: 47 } },
    { path: "w48.md", recipe: { kind: "repeat", unit: "a", count: 48 } },
    { path: "w49.md", recipe: { kind: "repeat", unit: "b", count: 49 } },
    { path: "small.md", recipe: { kind: "vocab", seed: 1, bytes: 2000 } },
    { path: "medium.md", recipe: { kind: "vocab", seed: 2, bytes: 20000 } },
    { path: "unit96.md", recipe: { kind: "vocab", seed: 3, bytes: 150000 } },
    { path: "large.md", recipe: { kind: "vocab", seed: 4, bytes: 1 * MB } },
    { path: "under4mb.md", recipe: { kind: "vocab", seed: 5, bytes: 4 * MB - 64 } },
    { path: "over4mb.md", recipe: { kind: "vocab", seed: 6, bytes: 4 * MB + 10 } },
    { path: "twobyte.md", recipe: { kind: "repeat", unit: "é", count: 30000 } },
    { path: "threebyte.md", recipe: { kind: "repeat", unit: "日本語", count: 9000 } },
    { path: "fourbyte.md", recipe: { kind: "repeat", unit: "😀a😀", count: 7000 } },
    { path: "mixed.md", recipe: { kind: "vocab", seed: 7, bytes: 60000 } },
    { path: "lowentropy.md", recipe: { kind: "repeat", unit: "a", count: 100000 } },
    { path: "crlf.txt", recipe: { kind: "repeat", unit: "line with crlf\r\n", count: 2000 } },
    { path: "data.csv", recipe: { kind: "vocab", seed: 8, bytes: 30000 } },
    { path: "board.canvas", recipe: { kind: "vocab", seed: 9, bytes: 12000 } },
    { path: "page.html", recipe: { kind: "vocab", seed: 10, bytes: 9000 } },
    { path: "minchunk.md", recipe: { kind: "vocab", seed: 11, bytes: 50000 }, minimumChunkSize: 3000 },
    { path: "cs0.md", recipe: { kind: "vocab", seed: 12, bytes: 300000 }, customChunkSize: 0 },
    { path: "bom-short.md", recipe: { kind: "literal", text: "﻿# short with BOM\n" } },
    { path: "bom-long.md", recipe: { kind: "vocab", seed: 13, bytes: 5000, prefix: "﻿" } },
    // binary
    { path: "empty.png", recipe: { kind: "random", seed: 20, bytes: 0 } },
    { path: "one.bin", recipe: { kind: "random", seed: 21, bytes: 1 } },
    { path: "small.png", recipe: { kind: "random", seed: 22, bytes: 100 * 1024 } },
    { path: "photo.jpg", recipe: { kind: "random", seed: 23, bytes: 3 * MB } },
    { path: "video.mp4", recipe: { kind: "random", seed: 24, bytes: 10 * MB } },
    { path: "zeros.bin", recipe: { kind: "fill", value: 0, bytes: 5 * MB } },
    { path: "cs0.pdf", recipe: { kind: "random", seed: 25, bytes: 1 * MB }, customChunkSize: 0 },
    { path: "cs0zeros.bin", recipe: { kind: "fill", value: 0, bytes: 1 * MB }, customChunkSize: 0 },
    { path: "cs1.pdf", recipe: { kind: "random", seed: 26, bytes: 700 * 1024 }, customChunkSize: 1 },
    { path: "data.json", recipe: { kind: "vocab", seed: 27, bytes: 40000 } },
];
const splitOut = [];
for (const c of splitCases) {
    const customChunkSize = c.customChunkSize ?? 60;
    const minimumChunkSize = c.minimumChunkSize ?? 20;
    const splitSettings = {
        ...settings,
        chunkSplitterVersion: "v3-rabin-karp",
        customChunkSize,
        minimumChunkSize,
        disableWorkerForGeneratingChunks: true,
    };
    const splitter = new ContentSplitter({ settingService: { currentSettings: () => splitSettings } });
    let input = makeInput(c.recipe);
    if (c.recipe.prefix) input = { text: c.recipe.prefix + input.text };
    // As the CLI reads files: plain-text extensions as strings, everything else as bytes.
    const text = isPlainTextUtil(c.path);
    let data;
    let bytes;
    if (input.text !== undefined && text) {
        data = createTextBlob(input.text);
        bytes = enc.encode(input.text);
    } else if (input.text !== undefined) {
        // e.g. `.json`: text content, but not a plain-text extension, so stored as binary
        bytes = enc.encode(input.text);
        data = createBinaryBlob(bytes);
    } else {
        if (text) throw new Error(`${c.path}: binary recipe for a text path`);
        data = createBinaryBlob(input.bytes);
        bytes = input.bytes;
    }
    const gen = await splitter.splitContent({ path: c.path, data, ctime: 0, mtime: 0, size: bytes.length });
    const pieces = [];
    for await (const piece of gen) {
        if (piece.length === 0) continue; // EntryManager skips empty pieces
        pieces.push({ len: text ? enc.encode(piece).length : Buffer.from(piece, "base64").length, id: "h:" + (await hm.computeHash(piece)) });
    }
    splitOut.push({
        path: c.path,
        recipe: c.recipe,
        customChunkSize,
        minimumChunkSize,
        type: text ? "plain" : "newnote",
        inputBytes: bytes.length,
        inputFnv1a: fnv1a(bytes),
        pieces,
    });
}
emit("splitter.json", splitOut);
