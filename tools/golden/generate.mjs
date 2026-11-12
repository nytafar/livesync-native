// Emits golden test vectors from @vrtmrz/livesync-commonlib (pinned in package.json)
// for the Rust crates. Usage: node generate.mjs <out-dir>
//
// Fixtures:
//   path2id.json      path -> document ID (path2id_base, no obfuscation)
//   chunk_ids.json    piece -> chunk ID (HashManager, hashAlg=xxhash64, encryption off)
//   decompress.json   compressed `data` field -> decompressed `data` (decompressDoc)
//   classify.json     path -> shouldBeIgnored / isPlainText
//   ignore_*.json     ignore-file decisions (isAccepted, isAcceptedAll); see ignore.mjs
//   splitter.json     v3-rabin-karp splits (ContentSplitter, as EntryManager uses it):
//                     inputs are recipes both sides regenerate, outputs are piece
//                     lengths (in UTF-8 bytes) and chunk IDs
//   import_settings.json  plugin data.json and CLI settings.json files as
//                     upstream's SettingService saves them; see settings.mjs
//   merge.json        base/left/right (+ mtimes) -> ConflictManager's automatic merge
//                     (`.md` line merge, `.json`/`.canvas` object merge), or null
//   line_diff.json    diff_match_patch line-mode diffs, as mergeSensibly computes them
import { mkdirSync, writeFileSync, readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";

import { path2id_base, shouldBeIgnored, isPlainText } from "@vrtmrz/livesync-commonlib/compat/string_and_binary/path";
import { HashManager } from "@vrtmrz/livesync-commonlib/hashing";
import { generateIgnore } from "./ignore.mjs";
import { generateSettings } from "./settings.mjs";
import { generateCrypto } from "./crypto.mjs";

// compress.js is not in the package's export map; import it by file URL.
const here = dirname(fileURLToPath(import.meta.url));
const pkgDir = join(here, "node_modules", "@vrtmrz", "livesync-commonlib");
const pkg = JSON.parse(readFileSync(join(pkgDir, "package.json"), "utf8"));
const { compressDoc, decompressDoc } = await import(pathToFileURL(join(pkgDir, "dist", "pouchdb", "compress.js")).href);

// The installed packages must be the upstream pin (../../upstream.env, KEY=value
// lines), or the fixtures would claim a release they do not come from.
const pin = Object.fromEntries(
    readFileSync(join(here, "..", "..", "upstream.env"), "utf8")
        .split("\n")
        .map((l) => l.trim())
        .filter((l) => l && !l.startsWith("#"))
        .map((l) => l.split("=", 2))
);
const installed = (name) => JSON.parse(readFileSync(join(here, "node_modules", name, "package.json"), "utf8")).version;
for (const [name, key] of [
    ["@vrtmrz/livesync-commonlib", "COMMONLIB"],
    ["octagonal-wheels", "OCTAGONAL_WHEELS"],
]) {
    if (installed(name) !== pin[key]) {
        console.error(`${name} ${installed(name)} is installed, but upstream.env pins ${key}=${pin[key]}; run npm ci (or scripts/bump-upstream.sh)`);
        process.exit(1);
    }
}

const outDir = process.argv[2] ?? join(here, "..", "..", "fixtures", "golden");
mkdirSync(outDir, { recursive: true });
const meta = { generator: "tools/golden/generate.mjs", package: pkg.name, version: pkg.version };
const emit = (name, cases, extra = {}) => {
    writeFileSync(join(outDir, name), JSON.stringify({ meta, ...extra, cases }, null, 1) + "\n");
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

await generateIgnore(emit);
await generateCrypto(emit);

{
    const { extra, files } = await generateSettings();
    const cases = Object.entries(files).map(([name, data]) => ({ name, data }));
    emit("import_settings.json", cases, extra);
}

// ---------- merge (ConflictManager) ----------
// Upstream's merge runs inside ConflictManager, which reads leaves through an
// EntryManager and a PouchDB. The smallest faithful harness is the one upstream's
// own unit tests use: stub `entryManager.getDBEntry` with plain leaves and
// `database.get` with `_revs_info` naming `1-base` as available on both branches,
// then call `tryAutoMergeSensibly` unmodified. It picks the line merge for `.md`
// and the object merge for `.json`/`.canvas`, as in the plugin. `null` means
// upstream does not merge (manual).
const { ConflictManager } = await import(pathToFileURL(join(pkgDir, "dist", "managers", "ConflictManager.js")).href);
const pkgRequire = createRequire(join(pkgDir, "package.json"));
const { setGlobalLogFunction } = await import("octagonal-wheels/common/logger");
setGlobalLogFunction(() => {}); // ConflictManager logs every merge; keep stdout to the summary
const dmpModule = pkgRequire("diff-match-patch");
const DiffMatchPatch = dmpModule.diff_match_patch ?? dmpModule;

async function upstreamMerge(path, base, left, right) {
    const leaves = { "1-base": base, "2-left": left, "2-right": right };
    const cm = new ConflictManager({
        entryManager: {
            getDBEntry: async (p, opt) => {
                const leaf = leaves[opt?.rev];
                if (!leaf) return false;
                return { _id: p, _rev: opt.rev, path: p, type: "plain", datatype: "plain", data: [leaf.data], mtime: leaf.mtime, ctime: leaf.mtime, deleted: false };
            },
        },
        pathService: { path2id: async (p) => p },
        database: {
            get: async (id, opt) => ({
                _id: id,
                _rev: opt.rev,
                _revs_info: [
                    { rev: opt.rev, status: "available" },
                    { rev: "1-base", status: "available" },
                ],
            }),
        },
    });
    const r = await cm.tryAutoMergeSensibly(path, { _rev: "2-left" }, ["2-right"]);
    return r === false ? null : r.result;
}

const mergeCases = [];
async function mergeCase(name, path, base, left, right, leftMtime = 2000, rightMtime = 3000) {
    if (mergeCases.some((c) => c.name === name)) throw new Error("duplicate merge case " + name);
    const result = await upstreamMerge(path, { data: base, mtime: 1000 }, { data: left, mtime: leftMtime }, { data: right, mtime: rightMtime });
    mergeCases.push({ name, path, base, left, right, leftMtime, rightMtime, result });
}
// Both mtime orders, so the "older first" rule is pinned either way.
async function mergeBoth(name, path, base, left, right) {
    await mergeCase(name, path, base, left, right, 2000, 3000);
    await mergeCase(name + "/left-newer", path, base, left, right, 3000, 2000);
}

// --- line merge, hand-picked ---
const L = (...lines) => lines.map((l) => l + "\n").join("");
await mergeBoth("lines/left-only", "note.md", L("a", "b", "c"), L("a", "B", "c"), L("a", "b", "c"));
await mergeBoth("lines/right-only", "note.md", L("a", "b", "c"), L("a", "b", "c"), L("a", "b", "C"));
await mergeBoth("lines/different-lines", "note.md", L("one", "two", "three", "four"), L("one", "TWO", "three", "four"), L("one", "two", "three", "FOUR"));
await mergeBoth("lines/same-change", "note.md", L("a", "b", "c"), L("a", "X", "c"), L("a", "X", "c"));
await mergeBoth("lines/change-vs-change", "note.md", L("a", "b", "c"), L("a", "X", "c"), L("a", "Y", "c"));
await mergeBoth("lines/delete-vs-change", "note.md", L("a", "b", "c"), L("a", "c"), L("a", "Y", "c"));
await mergeBoth("lines/delete-vs-unchanged", "note.md", L("a", "b", "c"), L("a", "c"), L("a", "b", "c", "d"));
await mergeBoth("lines/same-delete", "note.md", L("a", "b", "c"), L("a", "c"), L("a", "c"));
await mergeBoth("lines/same-delete-one-replaces", "note.md", L("a", "b", "c"), L("a", "c"), L("a", "B2", "c"));
await mergeBoth("lines/append-both", "daily.md", L("# Log", "- 09:00 start"), L("# Log", "- 09:00 start", "- 10:00 left"), L("# Log", "- 09:00 start", "- 11:00 right"));
await mergeBoth("lines/append-both-runs", "daily.md", L("# Log"), L("# Log", "left one", "left two"), L("# Log", "right one", "right two"));
await mergeBoth("lines/append-same", "note.md", L("a"), L("a", "same"), L("a", "same"));
await mergeBoth("lines/append-same-then-more", "note.md", L("a"), L("a", "same"), L("a", "same", "more"));
await mergeBoth("lines/insert-top-both", "note.md", L("a", "b"), L("top left", "a", "b"), L("top right", "a", "b"));
await mergeBoth("lines/same-insertion-shifted", "note.md", L("a"), L("a", "X"), L("a", "Y", "X"));
await mergeBoth("lines/same-insertion-shifted-2", "note.md", L("a", "z"), L("a", "B", "C", "z"), L("a", "C", "z"));
await mergeBoth("lines/same-insertion-swapped", "note.md", L("a"), L("a", "P", "Q"), L("a", "Q", "P"));
await mergeBoth("lines/trailing-delete", "note.md", L("a", "b", "c"), L("a", "b"), L("a", "B", "c"));
await mergeBoth("lines/trailing-delete-alone", "note.md", L("a", "b", "c"), L("a", "b"), L("a", "b", "c"));
await mergeBoth("lines/leading-delete", "note.md", L("a", "b", "c"), L("b", "c"), L("a", "b", "C"));
await mergeBoth("lines/no-trailing-newline-append", "note.md", "a\nb", "a\nb\nleft", "a\nb\nright");
await mergeBoth("lines/no-trailing-newline-edit", "note.md", "a\nb\nc", "A\nb\nc", "a\nb\nC");
await mergeBoth("lines/add-trailing-newline", "note.md", "a\nb", "a\nb\n", "A\nb");
await mergeBoth("lines/crlf-edit", "note.md", "a\r\nb\r\nc\r\n", "A\r\nb\r\nc\r\n", "a\r\nb\r\nC\r\n");
await mergeBoth("lines/crlf-append", "note.md", "a\r\nb\r\n", "a\r\nb\r\nleft\r\n", "a\r\nb\r\nright\r\n");
await mergeBoth("lines/crlf-vs-lf", "note.md", "a\r\nb\r\n", "a\nb\n", "a\r\nb\r\nc\r\n");
await mergeBoth("lines/empty-base", "note.md", "", L("left"), L("right"));
await mergeBoth("lines/empty-base-same", "note.md", "", L("same"), L("same"));
await mergeBoth("lines/emptied-vs-edit", "note.md", L("a", "b"), "", L("a", "B"));
await mergeBoth("lines/emptied-vs-unchanged", "note.md", L("a", "b"), "", L("a", "b", "c"));
await mergeBoth("lines/both-emptied", "note.md", L("a", "b"), "", "");
// Placement by base position, and runs that share lines.
await mergeBoth("lines/insert-after-replaced-range", "note.md", L("a", "b", "c"), L("a", "B", "C"), L("a", "b", "c", "d"));
await mergeBoth("lines/insert-before-replaced-range", "note.md", L("a", "b", "c"), L("a", "B", "C"), L("a", "x", "b", "c"));
await mergeBoth("lines/checkbox-sub-item", "todo.md", L("- [ ] task", "- [ ] next"), L("- [x] task", "- [ ] next"), L("- [ ] task", "  - detail", "- [ ] next"));
await mergeBoth("lines/log-done-and-append", "daily.md", L("# Log", "- 09 start"), L("# Log", "- 09 start (done)"), L("# Log", "- 09 start", "- 10 call"));
await mergeBoth("lines/deviation-replaced-shared-first-line", "note.md", L("# M", "Meeting TBD", "end"), L("# M", "Meeting at 10", "Room 1", "end"), L("# M", "Meeting at 10", "Room 2", "end"));
await mergeBoth("lines/insert-inside-replaced-range", "note.md", L("a", "b", "c", "d"), L("a", "X", "Y", "d"), L("a", "b", "new", "c", "d"));
await mergeBoth("lines/deviation-fences", "note.md", L("x"), L("x", "```", "A", "```"), L("x", "```", "B", "```"));
await mergeBoth("lines/deviation-paragraphs", "daily.md", L("# Log"), L("# Log", "", "Para A"), L("# Log", "", "Para B"));
await mergeBoth("lines/overlapping-deletions", "note.md", L("a", "b", "c", "d", "e"), L("a", "d", "e"), L("a", "b", "e"));
await mergeBoth("lines/deviation-whitespace-emptied", "note.md", L("a", "b"), "\n\n", L("a", "b", "c"));
await mergeBoth("lines/blank-lines", "note.md", L("a", "", "b"), L("a", "", "", "b", "left"), L("right", "a", "", "b"));
await mergeBoth("lines/unicode", "note.md", L("æøå", "日本語", "😀"), L("ÆØÅ", "日本語", "😀"), L("æøå", "日本語", "🎉"));
await mergeBoth("lines/frontmatter-as-text", "note.md", L("---", "title: Note", "tags: a", "---", "body"), L("---", "title: Note", "tags: b", "---", "body"), L("---", "title: Note", "tags: a", "---", "body", "more"));
await mergeBoth("lines/frontmatter-both-keys", "note.md", L("---", "title: Note", "tags: a", "---", "body"), L("---", "title: Left", "tags: a", "---", "body"), L("---", "title: Note", "tags: b", "---", "body"));
await mergeBoth("lines/replace-block", "note.md", L("a", "b", "c", "d", "e"), L("a", "x", "y", "e"), L("a", "b", "c", "d", "e", "f"));
await mergeBoth("lines/replace-block-longer", "note.md", L("a", "b", "c"), L("a", "x", "y", "z", "c"), L("A", "b", "c"));
await mergeCase("lines/equal-mtime", "note.md", L("a"), L("a", "left"), L("a", "right"), 2000, 2000);
await mergeBoth("lines/repeated-lines", "note.md", L("x", "x", "x"), L("x", "x", "x", "x"), L("x", "y", "x", "x"));
// Where this crate deviates on purpose: whole insertion runs, carried trailing deletions.
await mergeBoth("lines/deviation-runs-log", "daily.md", L("# Log", "- 09:00 start", "## Notes"), L("# Log", "- 09:00 start", "- 10:00 call", "  with notes", "## Notes"), L("# Log", "- 09:00 start", "- 11:00 review", "  - one", "  - two", "## Notes"));
// The one-line run first: whole runs and upstream's pairing agree.
await mergeCase("lines/deviation-runs-uneven", "note.md", L("a"), L("a", "l1", "l2", "l3"), L("a", "r1"));
await mergeCase("lines/runs-uneven-short-first", "note.md", L("a"), L("a", "l1", "l2", "l3"), L("a", "r1"), 3000, 2000);
await mergeBoth("lines/deviation-runs-shared-tail", "note.md", L("a"), L("a", "l1", "l2", "X"), L("a", "r1", "r2", "X"));
await mergeCase("lines/deviation-runs-unterminated", "note.md", L("a"), "a\nl1\nl2", L("a", "r1"));
await mergeCase("lines/runs-unterminated-last", "note.md", L("a"), "a\nl1\nl2", L("a", "r1"), 3000, 2000);
await mergeBoth("lines/deviation-trailing-delete-vs-change", "note.md", L("a", "b", "c"), L("a", "b"), L("a", "b", "C"));
await mergeBoth("lines/deviation-trailing-delete-both", "note.md", L("a", "b", "c"), L("A", "b"), L("a", "b"));
await mergeBoth("lines/deviation-trailing-delete-and-append", "note.md", L("a", "b", "c"), L("a", "b"), L("a", "b", "c", "d"));
await mergeBoth("lines/deviation-guard-stale-writer", "todo.md", L("# Todo", "- a", "- b", "- c"), L("new", "# Todo", "- a", "- b", "- c"), L("# Todo", "- a", "- b"));
await mergeBoth("lines/deviation-guard-fresh-writer", "todo.md", L("# Todo", "- a", "- b", "- c"), L("new", "# Todo", "- a", "- b"), L("# Todo", "- a", "- b"));

// --- line merge, generated: small vocabulary so lines repeat and alignment matters ---
const WORDS = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "- [ ] task", "- [x] done", "", "# Heading", "> quote", "golf"];
const mr = xorshift(0x5eed1234);
const pick = (arr) => arr[mr() % arr.length];
function randomLines(n) {
    return Array.from({ length: n }, () => pick(WORDS));
}
function mutate(lines, edits) {
    const out = [...lines];
    for (let i = 0; i < edits; i++) {
        const op = mr() % 4;
        const at = out.length ? mr() % (out.length + 1) : 0;
        if (op === 0 || out.length === 0) out.splice(at, 0, pick(WORDS) + (mr() % 3 === 0 ? " " + (mr() % 100) : ""));
        else if (op === 1) out.splice(Math.min(at, out.length - 1), 1);
        else if (op === 2) out[Math.min(at, out.length - 1)] = pick(WORDS) + " " + (mr() % 100);
        else out.push(pick(WORDS) + " " + (mr() % 100));
    }
    return out;
}
const join_ = (lines, eol, final) => lines.join(eol) + (final && lines.length ? eol : "");
for (let i = 0; i < 400; i++) {
    const big = i >= 340;
    const base = randomLines(big ? 60 + (mr() % 240) : mr() % 14);
    const left = mutate(base, 1 + (mr() % (big ? 12 : 4)));
    const right = mutate(base, 1 + (mr() % (big ? 12 : 4)));
    const eol = mr() % 5 === 0 ? "\r\n" : "\n";
    const finals = [mr() % 6 !== 0, mr() % 6 !== 0, mr() % 6 !== 0];
    const lm = 2000 + (mr() % 3) * 500;
    const rm = 2000 + (mr() % 3) * 500;
    await mergeCase(`lines/random/${i}`, "random.md", join_(base, eol, finals[0]), join_(left, eol, finals[1]), join_(right, eol, finals[2]), lm, rm);
}

// --- object merge, hand-picked ---
const J = (v) => JSON.stringify(v);
const JI = (v, indent = 2) => JSON.stringify(v, null, indent) + "\n";
const canvasBase = {
    nodes: [
        { id: "n1", type: "text", text: "First", x: 0, y: 0, width: 200, height: 60 },
        { id: "n2", type: "text", text: "Second", x: 300, y: 0, width: 200, height: 60 },
    ],
    edges: [{ id: "e1", fromNode: "n1", toNode: "n2" }],
};
const withNodes = (extra, edit = (n) => n) => ({ ...canvasBase, nodes: [...canvasBase.nodes.map(edit), ...extra] });
await mergeBoth("object/canvas-add-both", "board.canvas", JI(canvasBase, "\t"), JI(withNodes([{ id: "n3", type: "text", text: "Left", x: 0, y: 100, width: 200, height: 60 }]), "\t"), JI(withNodes([{ id: "n4", type: "text", text: "Right", x: 300, y: 100, width: 200, height: 60 }]), "\t"));
await mergeBoth("object/canvas-edit-and-add", "board.canvas", JI(canvasBase, "\t"), JI(withNodes([], (n) => (n.id === "n1" ? { ...n, text: "First edited" } : n)), "\t"), JI(withNodes([{ id: "n4", type: "text", text: "Right", x: 300, y: 100, width: 200, height: 60 }]), "\t"));
await mergeBoth("object/canvas-same-node-different", "board.canvas", JI(canvasBase, "\t"), JI(withNodes([], (n) => (n.id === "n1" ? { ...n, text: "Left text" } : n)), "\t"), JI(withNodes([], (n) => (n.id === "n1" ? { ...n, text: "Right text" } : n)), "\t"));
await mergeBoth("object/canvas-same-node-other-fields", "board.canvas", JI(canvasBase, "\t"), JI(withNodes([], (n) => (n.id === "n1" ? { ...n, text: "Moved and edited" } : n)), "\t"), JI(withNodes([], (n) => (n.id === "n1" ? { ...n, x: 40, y: 40 } : n)), "\t"));
await mergeBoth("object/canvas-delete-vs-edit-node", "board.canvas", JI(canvasBase, "\t"), JI({ ...canvasBase, nodes: [canvasBase.nodes[1]] }, "\t"), JI(withNodes([], (n) => (n.id === "n1" ? { ...n, text: "Edited" } : n)), "\t"));
await mergeBoth("object/canvas-delete-node-and-add", "board.canvas", JI(canvasBase, "\t"), JI({ ...canvasBase, nodes: [canvasBase.nodes[1]] }, "\t"), JI(withNodes([{ id: "n5", type: "text", text: "New", x: 0, y: 200, width: 100, height: 60 }]), "\t"));
await mergeBoth("object/app-json-keys", "app.json", JI({ theme: "dark", fontSize: 16, spellcheck: true }), JI({ theme: "light", fontSize: 16, spellcheck: true }), JI({ theme: "dark", fontSize: 16, spellcheck: false, newKey: "x" }));
await mergeBoth("object/same-key-different", "app.json", JI({ theme: "dark" }), JI({ theme: "light" }), JI({ theme: "system" }));
await mergeBoth("object/same-key-same", "app.json", JI({ theme: "dark", a: 1 }), JI({ theme: "light", a: 1 }), JI({ theme: "light", a: 2 }));
await mergeBoth("object/delete-key", "app.json", JI({ a: 1, b: 2, c: 3 }), JI({ a: 1, c: 3 }), JI({ a: 1, b: 2, c: 4 }));
await mergeBoth("object/delete-vs-change-key", "app.json", JI({ a: 1, b: 2 }), JI({ a: 1 }), JI({ a: 1, b: 3 }));
await mergeBoth("object/nested", "data.json", JI({ settings: { a: 1, b: { c: 2, d: 3 } }, list: [1, 2] }), JI({ settings: { a: 1, b: { c: 20, d: 3 } }, list: [1, 2] }), JI({ settings: { a: 1, b: { c: 2, d: 30 } }, list: [1, 2] }));
await mergeBoth("object/plain-array-one-side", "data.json", JI({ list: [1, 2], x: 1 }), JI({ list: [1, 2, 3], x: 1 }), JI({ list: [1, 2], x: 2 }));
await mergeBoth("object/plain-array-both", "data.json", JI({ list: [1, 2] }), JI({ list: [1, 2, 3] }), JI({ list: [0, 1, 2] }));
await mergeBoth("object/plain-array-both-same", "data.json", JI({ list: [1, 2] }), JI({ list: [1, 2, 3] }), JI({ list: [1, 2, 3] }));
await mergeBoth("object/nested-id-arrays", "data.json",
    JI({ groups: [{ id: "g1", items: [{ id: "i1", v: 1 }, { id: "i2", v: 2 }] }, { id: "g2", items: [] }] }),
    JI({ groups: [{ id: "g1", items: [{ id: "i1", v: 10 }, { id: "i2", v: 2 }] }, { id: "g2", items: [] }] }),
    JI({ groups: [{ id: "g1", items: [{ id: "i1", v: 1 }, { id: "i2", v: 2 }, { id: "i3", v: 3 }] }, { id: "g2", items: [{ id: "i9", v: 9 }] }] }));
await mergeBoth("object/nested-id-arrays-conflict", "data.json",
    JI({ groups: [{ id: "g1", items: [{ id: "i1", v: 1 }] }] }),
    JI({ groups: [{ id: "g1", items: [{ id: "i1", v: 10 }] }] }),
    JI({ groups: [{ id: "g1", items: [{ id: "i1", v: 11 }] }] }));
await mergeBoth("object/empty-array-to-ids", "data.json", JI({ list: [] }), JI({ list: [{ id: "a", v: 1 }] }), JI({ list: [{ id: "b", v: 2 }] }));
await mergeBoth("object/type-change", "data.json", JI({ a: { b: 1 }, c: 1 }), JI({ a: "flat", c: 1 }), JI({ a: { b: 1 }, c: 2 }));
await mergeBoth("object/null-values", "data.json", JI({ a: null, b: 1 }), JI({ a: 5, b: 1 }), JI({ a: null, b: 2 }));
await mergeBoth("object/to-null", "data.json", JI({ a: 1, b: 1 }), JI({ a: null, b: 1 }), JI({ a: 1, b: 2 }));
await mergeBoth("object/compact", "data.json", J({ a: 1, b: 2 }), J({ a: 10, b: 2 }), J({ a: 1, b: 20 }));
await mergeBoth("object/no-trailing-newline", "data.json", JSON.stringify({ a: 1, b: 2 }, null, 2), JSON.stringify({ a: 10, b: 2 }, null, 2), JSON.stringify({ a: 1, b: 20 }, null, 2));
await mergeBoth("object/four-space", "data.json", JI({ a: 1, b: { c: [1, 2] } }, 4), JI({ a: 2, b: { c: [1, 2] } }, 4), JI({ a: 1, b: { c: [1, 2], d: true } }, 4));
await mergeBoth("object/top-level-array-ids", "data.json", JI([{ id: "a", v: 1 }, { id: "b", v: 2 }]), JI([{ id: "a", v: 10 }, { id: "b", v: 2 }]), JI([{ id: "a", v: 1 }, { id: "b", v: 2 }, { id: "c", v: 3 }]));
await mergeBoth("object/unicode-strings", "data.json", JI({ s: "æøå", t: "x" }), JI({ s: "日本語 \"quoted\" \\ \u0001", t: "x" }), JI({ s: "æøå", t: "😀" }));
await mergeBoth("object/floats", "data.json", JI({ a: 1.5, b: 0.1, c: 1e21, d: -0 }), JI({ a: 2.25, b: 0.1, c: 1e21, d: -0 }), JI({ a: 1.5, b: 0.1, c: 1e21, d: 1e-7 }));
await mergeBoth("object/add-same-key-both", "data.json", JI({ a: 1 }), JI({ a: 1, n: { x: 1 } }), JI({ a: 1, n: { y: 2 } }));
await mergeBoth("object/deviation-invalid-json", "data.json", JI({ a: 1 }), "{ not json", JI({ a: 1, b: 2 }));
await mergeBoth("object/deviation-loose-equality", "data.json", JI({ a: 0 }), JI({ a: 1 }), JI({ a: "1" }));
await mergeBoth("object/deviation-delete-parent-vs-edit-child", "data.json", JI({ p: { a: 1, b: 2 }, q: 1 }), JI({ q: 1 }), JI({ p: { a: 1, b: 3 }, q: 1 }));
await mergeBoth("object/deviation-null-parent-vs-edit-child", "data.json", JI({ p: { a: 1 }, q: 1 }), JI({ p: null, q: 1 }), JI({ p: { a: 2 }, q: 1 }));
await mergeBoth("object/deviation-swap-both", "data.json", JI({ p: "text" }), JI({ p: { x: 1 } }), JI({ p: { y: 2 } }));
await mergeBoth("object/deviation-numeric-ids", "data.json", JI({ list: [{ id: "2", v: 1 }, { id: "1", v: 1 }] }), JI({ list: [{ id: "2", v: 5 }, { id: "1", v: 1 }] }), JI({ list: [{ id: "2", v: 1 }, { id: "1", v: 1 }, { id: "x", v: 3 }] }));
await mergeBoth("object/numeric-keys", "data.json", JI({ b: 1, 10: "ten", 2: "two" }), JI({ b: 2, 10: "ten", 2: "two" }), JI({ b: 1, 10: "ten", 2: "zwei" }));
await mergeBoth("object/big-integer", "data.json", '{\n  "big": 12345678901234567890,\n  "a": 1\n}\n', '{\n  "big": 12345678901234567890,\n  "a": 2\n}\n', '{\n  "big": 12345678901234567890,\n  "a": 1,\n  "b": 1\n}\n');

// --- object merge, generated ---
const KEYS = ["a", "b", "c", "d", "e", "list", "nodes", "cfg"];
function randomValue(depth) {
    const k = mr() % (depth > 1 ? 4 : 7);
    if (k === 0) return mr() % 10;
    if (k === 1) return pick(["x", "y", "z", "longer text"]);
    if (k === 2) return mr() % 2 === 0;
    if (k === 3) return null;
    if (k === 4) return Array.from({ length: mr() % 3 }, () => mr() % 5);
    if (k === 5) return Array.from({ length: 1 + (mr() % 3) }, (_, i) => ({ id: "id" + ((mr() % 4) + i * 4), v: mr() % 5 }));
    return randomObject(depth + 1);
}
function randomObject(depth = 0) {
    const o = {};
    for (let i = 0, n = 1 + (mr() % 4); i < n; i++) o[pick(KEYS)] = randomValue(depth);
    return o;
}
function mutateObject(o, depth = 0) {
    const out = structuredClone(o);
    const keys = Object.keys(out);
    const op = mr() % 5;
    if (op === 0 || keys.length === 0) out[pick(KEYS)] = randomValue(depth + 1);
    else if (op === 1) delete out[pick(keys)];
    else {
        const k = pick(keys);
        const v = out[k];
        if (v && typeof v === "object" && !Array.isArray(v)) out[k] = mutateObject(v, depth + 1);
        else if (Array.isArray(v) && v.length && typeof v[0] === "object") {
            const i = mr() % v.length;
            if (mr() % 2) v[i] = { ...v[i], v: mr() % 5 };
            else v.push({ id: "new" + (mr() % 6), v: mr() % 5 });
        } else out[k] = randomValue(depth + 1);
    }
    return out;
}
for (let i = 0; i < 150; i++) {
    const base = randomObject();
    let left = mutateObject(base);
    let right = mutateObject(base);
    if (mr() % 2) left = mutateObject(left);
    await mergeCase(`object/random/${i}`, "random.json", JI(base), JI(left), JI(right), 2000 + (mr() % 2) * 1000, 2500);
}
emit("merge.json", mergeCases);

// ---------- line diff (diff_match_patch in line mode, as mergeSensibly runs it) ----------
const dmp = new DiffMatchPatch();
const lineDiff = (a, b) => {
    const m = dmp.diff_linesToChars_(a, b);
    const d = dmp.diff_main(m.chars1, m.chars2, false);
    dmp.diff_charsToLines_(d, m.lineArray);
    return d.map((x) => [x[0], x[1]]);
};
const diffCases = [];
for (const c of mergeCases) {
    if (!c.path.endsWith(".md")) continue;
    diffCases.push({ a: c.base, b: c.left, diffs: lineDiff(c.base, c.left) });
}
emit("line_diff.json", diffCases);
