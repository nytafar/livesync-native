// End-to-end encryption vectors (E2EEAlgorithm "v2", HKDF) from
// @vrtmrz/livesync-commonlib and octagonal-wheels, for crates/livesync-crypto
// and the encrypted chunk IDs in crates/livesync-model.
//
// Standalone:   node crypto.mjs [out-dir]        (default ../../fixtures/golden)
// From generate.mjs:  import { generateCrypto } from "./crypto.mjs";
//                     await generateCrypto(emit);
//
// Fixtures (each records the commonlib and octagonal-wheels versions in `upstream`):
//   crypto_chunk_ids.json    passphrase -> hashedPassphrase, piece -> `h:+` chunk ID
//   crypto_envelopes.json    `%=` envelopes upstream made (seeded randomness, so Rust
//                            can reproduce them byte for byte) and ones that must fail
//   crypto_documents.json    documents through upstream's `incoming` (encrypt for the
//                            remote) and `outgoing` (decrypt after reading)
//   crypto_policy.json       assertUnencryptedContentPolicy(doc, "v2", accept) verdicts
//   crypto_sync_params.json  `pbkdf2salt` strings -> the salt bytes upstream decodes
//
// Encryption draws its HKDF salt and IV from crypto.getRandomValues, which is
// replaced by a seeded xorshift32 (one byte per step) while a case runs; every
// case records its seed. All passphrases and salts are synthetic.
import { readFileSync, mkdirSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import { HashManager } from "@vrtmrz/livesync-commonlib/hashing";
import { setGlobalLogFunction } from "octagonal-wheels/common/logger";

const here = dirname(fileURLToPath(import.meta.url));
const pkgDir = join(here, "node_modules", "@vrtmrz", "livesync-commonlib");
const wheelsDir = join(here, "node_modules", "octagonal-wheels");
const pkg = JSON.parse(readFileSync(join(pkgDir, "package.json"), "utf8"));
const wheels = JSON.parse(readFileSync(join(wheelsDir, "package.json"), "utf8"));
const lib = (p) => import(pathToFileURL(join(pkgDir, "dist", p)).href);

const { getConfiguredFunctionsForEncryption, encrypt: encryptV1 } = await lib("pouchdb/encryption.js");
const { assertUnencryptedContentPolicy } = await lib("pouchdb/encryption.policy.js");
const { base64ToArrayBufferInternalBrowser } = await import("octagonal-wheels/binary/base64");
const { encrypt: encryptHKDF, decrypt: decryptHKDF } = await import("octagonal-wheels/encryption/hkdf");

export const upstream = { commonlib: pkg.version, octagonalWheels: wheels.version };

// ---------- seeded randomness ----------
function xorshift(seed) {
    let s = seed >>> 0;
    return (arr) => {
        const u8 = new Uint8Array(arr.buffer, arr.byteOffset, arr.byteLength);
        for (let i = 0; i < u8.length; i++) {
            s ^= s << 13;
            s >>>= 0;
            s ^= s >>> 17;
            s ^= s << 5;
            s >>>= 0;
            u8[i] = s & 0xff;
        }
        return arr;
    };
}
async function seeded(seed, fn) {
    const had = Object.getOwnPropertyDescriptor(globalThis.crypto, "getRandomValues");
    globalThis.crypto.getRandomValues = xorshift(seed);
    try {
        return await fn();
    } finally {
        if (had) Object.defineProperty(globalThis.crypto, "getRandomValues", had);
        else delete globalThis.crypto.getRandomValues;
    }
}
const bytes = (seed, n) => xorshift(seed)(new Uint8Array(n));
const b64 = (u8) => Buffer.from(u8).toString("base64");

// ---------- keys (synthetic) ----------
const KEYS = {
    main: { passphrase: "synthetic-e2ee-passphrase", pbkdf2Salt: b64(bytes(0x51a7, 32)) },
    unicode: { passphrase: "Ærlig talt – 日本語 😀 pass", pbkdf2Salt: b64(bytes(0x0dd5, 32)) },
    empty: { passphrase: "", pbkdf2Salt: b64(bytes(0x7e57, 32)) },
};
const saltOf = (k) => new Uint8Array(Buffer.from(KEYS[k].pbkdf2Salt, "base64"));

function describe(e) {
    return {
        name: e?.name ?? typeof e,
        message: String(e?.message ?? e),
        ...(e?.code ? { code: e.code } : {}),
        ...(e?.contentKind ? { contentKind: e.contentKind, documentId: e.documentId } : {}),
    };
}
async function outcome(fn) {
    try {
        return { ok: await fn() };
    } catch (e) {
        return { error: describe(e) };
    }
}
// What Rust must report for an upstream error.
function classify(err) {
    if (err.code === "UNENCRYPTED_CONTENT_REJECTED") {
        return { error: "rejected", contentKind: err.contentKind, documentId: err.documentId };
    }
    if (err.message.includes("Encrypted file content uses a representation")) {
        return { error: "unsupported", unsupported: "UnreadableEncryptedContent" };
    }
    if (err.message.includes("Unknown encryption version")) return { error: "unsupported", unsupported: "UnknownCiphertext" };
    if (err.name === "OperationError") return { error: "authentication" };
    if (err.name === "TypeError" || err.message.startsWith("Invalid")) return { error: "malformed" };
    throw new Error(`unclassified upstream error: ${err.name}: ${err.message}`);
}

// ---------- chunk IDs ----------
async function chunkIds() {
    const passphrases = [
        "",
        "a",
        "ab",
        "abc",
        "abcd",
        "abcde",
        "synthetic-e2ee-passphrase",
        "correct horse battery staple",
        "pässwörd ÆØÅ",
        "日本語のパスワード",
        "😀", // length 2: the 3/4 cut keeps 1 unit, half a surrogate pair
        "a😀", // length 3: keeps "a" and a lone high surrogate
        "ab😀cd😀",
        "😀😀😀😀",
        "-dash-",
        " leading and trailing ",
        "x".repeat(1000),
        "pass😀".repeat(50),
    ];
    // Pseudo-random passphrases over ASCII, Latin-1, CJK and astral planes, to
    // exercise the floating-point steps of mixedHash.
    const r = xorshift(0x5eed);
    const pick = (n) => r(new Uint32Array(n));
    for (let i = 0; i < 30; i++) {
        const [len] = pick(1);
        let s = "";
        for (const v of pick(1 + (len % 40))) {
            const range = v % 4;
            if (range === 0) s += String.fromCharCode(0x20 + (v >>> 8) % 0x5f);
            else if (range === 1) s += String.fromCharCode(0xa0 + (v >>> 8) % 0x160);
            else if (range === 2) s += String.fromCharCode(0x4e00 + (v >>> 8) % 0x5000);
            else s += String.fromCodePoint(0x1f300 + (v >>> 8) % 0x300);
        }
        passphrases.push(s);
    }
    const pieces = ["", "a", "hello world", "# Heading\n\nSome text.\n", "æøå", "😀🎉 pairs", "日本語", b64(bytes(7, 300))];
    const cases = [];
    for (const passphrase of passphrases) {
        const settings = { hashAlg: "xxhash64", encrypt: true, passphrase, usePathObfuscation: false };
        const hm = new HashManager({ settingService: { currentSettings: () => settings } });
        await hm.initialise();
        const ids = [];
        for (const piece of pieces) ids.push({ piece, id: "h:" + (await hm.computeHash(piece)) });
        cases.push({ passphrase, hashedPassphrase: hm.hashedPassphrase, ids });
    }
    return { cases };
}

// ---------- envelopes ----------
async function envelopes() {
    const plaintexts = [
        ["empty", ""],
        ["one char", "a"],
        ["ascii", "hello world"],
        ["unicode", "æøå ÆØÅ 日本語 😀🎉 Ærlig talt – ok"],
        ["128 units", "y".repeat(128)],
        ["129 units", "y".repeat(129)],
        ["128 units multibyte", "é".repeat(128)],
        ["nul and controls", "a\u0000b\u0001\t\n\r"],
        ["leading BOM", "﻿text after a BOM"],
        ["markdown", "# Title\n\n- item\n- [[link]]\n\n```js\nconsole.log(1)\n```\n"],
        ["1000 chars", "x".repeat(1000)],
        ["multibyte 600", "multi-byte ".repeat(30) + "ü".repeat(300)],
        ["json", JSON.stringify({ "h:+abc": { data: "inline", epoch: 3 } })],
        ["base64 16KiB", b64(bytes(99, 16384))],
    ];
    const cases = [];
    let seed = 1000;
    for (const key of Object.keys(KEYS)) {
        for (const [name, plaintext] of plaintexts) {
            const s = seed++;
            if (key !== "main" && !["empty", "unicode", "129 units", "1000 chars"].includes(name)) continue;
            const envelope = await seeded(s, () => encryptHKDF(plaintext, KEYS[key].passphrase, saltOf(key)));
            const back = await decryptHKDF(envelope, KEYS[key].passphrase, saltOf(key));
            if (back !== plaintext) throw new Error(`round trip failed: ${name}`);
            cases.push({ name: `${key}: ${name}`, key, seed: s, plaintext, envelope });
        }
    }
    // Failures. Each must fail upstream too.
    const base = cases.find((c) => c.key === "main" && c.name.endsWith(": unicode"));
    const raw = Buffer.from(base.envelope.slice(2), "base64");
    const flip = (i) => {
        const b = Buffer.from(raw);
        b[i < 0 ? b.length + i : i] ^= 0x01;
        return "%=" + b.toString("base64");
    };
    const failures = [
        { name: "wrong passphrase", key: "unicode", envelope: base.envelope, expect: "authentication" },
        { name: "wrong salt", key: "empty", envelope: base.envelope, expect: "authentication" },
        { name: "flipped IV bit", key: "main", envelope: flip(0), expect: "authentication" },
        { name: "flipped HKDF salt bit", key: "main", envelope: flip(20), expect: "authentication" },
        { name: "flipped ciphertext bit", key: "main", envelope: flip(50), expect: "authentication" },
        { name: "flipped tag bit", key: "main", envelope: flip(-1), expect: "authentication" },
        { name: "tag cut short", key: "main", envelope: "%=" + raw.subarray(0, raw.length - 1).toString("base64"), expect: "authentication" },
        { name: "only IV and salt", key: "main", envelope: "%=" + raw.subarray(0, 44).toString("base64"), expect: "authentication" },
        { name: "shorter than IV and salt", key: "main", envelope: "%=" + raw.subarray(0, 43).toString("base64"), expect: "malformed" },
        { name: "no prefix", key: "main", envelope: base.envelope.slice(2), expect: "malformed" },
        { name: "V1 prefix", key: "main", envelope: "%" + base.envelope.slice(2), expect: "malformed" },
        { name: "not base64", key: "main", envelope: "%=***not base64***", expect: "malformed" },
    ];
    for (const f of failures) {
        const r = await outcome(() => decryptHKDF(f.envelope, KEYS[f.key].passphrase, saltOf(f.key)));
        if (!r.error) throw new Error(`upstream decrypted the failure case ${f.name}`);
        f.upstreamError = r.error;
    }
    return { extra: { keys: KEYS }, cases, failures };
}

// ---------- documents ----------
const LOG = () => {};
function transforms(key, accept) {
    const salt = saltOf(key);
    return getConfiguredFunctionsForEncryption(KEYS[key].passphrase, false, false, async () => salt, "v2", false, LOG, accept);
}

async function documents() {
    const { incoming, outgoing } = transforms("main", false);
    const v1 = await seeded(77, () => encryptV1("v1 data", KEYS.main.passphrase, false));
    const v1Eden = await seeded(78, () => encryptV1(JSON.stringify({ "h:+e1": { data: "x", epoch: 1 } }), KEYS.main.passphrase, false));
    const meta = { ctime: 1700000000000, mtime: 1700000001000, size: 12 };
    // Local documents, as they would be written to the remote. Eden keys are
    // in sorted order, which is the order serde_json writes them in.
    const local = [
        ["chunk", { _id: "h:+abc123", type: "leaf", data: "hello world" }],
        ["chunk with rev", { _id: "h:+abc124", _rev: "1-0123456789abcdef", type: "leaf", data: "with a revision" }],
        ["chunk unicode", { _id: "h:+u1", type: "leaf", data: "æøå 日本語 😀🎉" }],
        ["chunk empty data", { _id: "h:+empty", type: "leaf", data: "" }],
        ["chunk large", { _id: "h:+large", type: "leaf", data: b64(bytes(5, 20000)) }],
        ["chunk e_ false", { _id: "h:+ef", type: "leaf", data: "marked false", e_: false }],
        ["syncinfo", { _id: "syncinfo", type: "syncinfo", data: "sync info payload" }],
        [
            "note with eden",
            { _id: "note.md", type: "plain", path: "note.md", children: ["h:+c1"], ...meta, eden: { "h:+e1": { data: "inline chunk", epoch: 1 } } },
        ],
        [
            "note with two eden chunks",
            {
                _id: "folder/two.md",
                type: "plain",
                path: "Folder/Two.md",
                children: [],
                ...meta,
                eden: { "h:+a": { data: "first 😀", epoch: 2 }, "h:+b": { data: "second", epoch: 3 } },
            },
        ],
        ["note with empty eden", { _id: "empty-eden.md", type: "plain", path: "empty-eden.md", children: ["h:+c1"], ...meta, eden: {} }],
        ["note without eden", { _id: "plain.md", type: "plain", path: "plain.md", children: ["h:+c1", "h:+c2"], ...meta }],
        ["binary note", { _id: "image.png", type: "newnote", path: "image.png", children: ["h:+c3"], ...meta, eden: {} }],
        ["deleted note", { _id: "gone.md", type: "plain", path: "gone.md", children: [], ...meta, deleted: true, eden: {} }],
        ["tombstone", { _id: "gone2.md", _rev: "3-abcdef", _deleted: true }],
        ["milestone (_local)", { _id: "_local/obsydian_livesync_milestone", type: "milestoneinfo", created: 1, accepted_nodes: ["n1"], node_chunk_info: {} }],
        ["version info", { _id: "obsydian_livesync_version", type: "versioninfo", version: 12 }],
        ["sync parameters", { _id: "_local/obsidian_livesync_sync_parameters", type: "sync-parameters", protocolVersion: 2, pbkdf2salt: KEYS.main.pbkdf2Salt }],
        ["unencrypted chunk id", { _id: "h:plain1", type: "leaf", data: "not an encrypted chunk id" }],
        ["legacy inline note", { _id: "legacy.md", type: "notes", path: "legacy.md", data: "inline body", ...meta }],
        ["internal file", { _id: "i:.obsidian/app.json", type: "newnote", path: "i:.obsidian/app.json", children: ["h:+c4"], ...meta, eden: {} }],
        // An array eden is not written by upstream, but `in`/`Object.keys` accept it:
        // it is encrypted, and reading spreads it back into an object keyed by index.
        ["note with eden array", { _id: "arr.md", type: "plain", path: "arr.md", children: [], ...meta, eden: [{ data: "x", epoch: 1 }] }],
    ];
    const cases = [];
    let seed = 5000;
    for (const [name, doc] of local) {
        const s = seed++;
        const inc = await seeded(s, () => outcome(() => incoming(structuredClone(doc))));
        const c = { name, seed: s, local: doc, incoming: inc.ok ? { ok: inc.ok } : { ...classify(inc.error), upstream: inc.error } };
        if (inc.ok) {
            const out = await outcome(() => outgoing(structuredClone(inc.ok)));
            c.outgoing = out.ok ? { ok: out.ok } : { ...classify(out.error), upstream: out.error };
        }
        cases.push(c);
    }
    // Incoming-only edge cases (upstream throws, leaves alone, or does what
    // this build does not support).
    const incomingOnly = [
        ["already encrypted chunk", { _id: "h:+enc", type: "leaf", data: cases[0].incoming.ok.data, e_: true }],
        ["chunk marked e_ with unknown data", { _id: "h:+unk", type: "leaf", data: "garbage", e_: true }],
        ["chunk tombstone without data", { _id: "h:+dead", _rev: "2-aa", _deleted: true }],
        ["chunk with non-string data", { _id: "h:+num", type: "leaf", data: null }],
        ["eden already encrypted", { _id: "pre.md", type: "plain", path: "pre.md", children: [], ...meta, eden: cases[7].incoming.ok.eden }],
        ["eden null", { _id: "null-eden.md", type: "plain", path: "null-eden.md", children: [], ...meta, eden: null }],
        [
            "chunk with V1 data",
            { _id: "h:+v1", type: "leaf", data: v1, e_: true },
            { error: "unsupported", unsupported: "V1Ciphertext", divergence: "upstream decrypts V1 and re-encrypts with HKDF" },
        ],
        [
            "obfuscated path",
            { _id: "f:0123abcd", type: "plain", path: "secret.md", children: [], ...meta },
            { error: "unsupported", unsupported: "PathObfuscation", divergence: "upstream encrypts the metadata into a /\\: envelope" },
        ],
    ];
    for (const [name, doc, rust] of incomingOnly) {
        const s = seed++;
        const inc = await seeded(s, () => outcome(() => incoming(structuredClone(doc))));
        const upstreamSide = inc.ok ? { ok: inc.ok } : { ...classify(inc.error), upstream: inc.error };
        cases.push({ name, seed: s, local: doc, incoming: rust ? { ...rust, upstreamResult: upstreamSide } : upstreamSide });
    }
    // Remote documents read through `outgoing`, with and without accepting
    // unencrypted content.
    const encChunk = cases[0].incoming.ok;
    const encEden = cases[7].incoming.ok;
    const remote = [
        ["encrypted chunk", encChunk],
        ["encrypted chunk, wrong passphrase", encChunk, "unicode"],
        ["plaintext chunk", { _id: "h:+p", type: "leaf", data: "plain" }],
        ["plaintext chunk without type", { _id: "h:+p2", data: "plain" }],
        ["plaintext chunk tombstone", { _id: "h:+p3", _deleted: true, data: "x" }],
        ["chunk e_ with unknown data", { _id: "h:+u", type: "leaf", data: "garbage", e_: true }],
        ["chunk e_ with non-string data", { _id: "h:+n", type: "leaf", data: 5, e_: true }],
        ["tampered chunk", { ...encChunk, data: encChunk.data.slice(0, -4) + (encChunk.data.endsWith("AAA=") ? "BBB=" : "AAA=") }],
        ["note with encrypted eden", encEden],
        ["note with plaintext eden", cases[7].local],
        ["note with encrypted and plaintext eden", { ...encEden, eden: { ...encEden.eden, "h:+plain": { data: "p", epoch: 4 } } }],
        ["note with empty eden", { _id: "e.md", type: "plain", path: "e.md", children: [], ...meta, eden: {} }],
        ["note with eden array", { _id: "arr.md", type: "plain", path: "arr.md", children: [], ...meta, eden: [] }],
        ["inline note", { _id: "legacy.md", type: "notes", path: "legacy.md", data: "inline", ...meta }],
        ["inline note marked e_", { _id: "legacy.md", type: "notes", path: "legacy.md", data: "%=AAAA", e_: true, ...meta }],
        ["deleted plain note with data", { _id: "d.md", type: "plain", deleted: true, data: "x", children: [], ...meta }],
        ["syncinfo plaintext", { _id: "syncinfo", type: "syncinfo", data: "plain sync info" }],
        ["syncinfo with other type", { _id: "syncinfo", type: "other", data: "plain sync info" }],
        ["syncinfo encrypted", cases.find((c) => c.name === "syncinfo").incoming.ok],
        ["versioninfo", { _id: "obsydian_livesync_version", type: "versioninfo", version: 12 }],
        ["_local doc with data", { _id: "_local/anything", data: "local" }],
        [
            "chunk with V1 data",
            { _id: "h:+v1", type: "leaf", data: v1, e_: true },
            "main",
            { error: "unsupported", unsupported: "V1Ciphertext", divergence: "upstream decrypts V1 ciphertext" },
        ],
        [
            "eden encrypted with V1",
            { _id: "v1eden.md", type: "plain", path: "v1eden.md", children: [], ...meta, eden: { "h:++encrypted": { data: v1Eden, epoch: 999999 } } },
            "main",
            { error: "unsupported", unsupported: "V1Eden", divergence: "upstream decrypts V1 eden" },
        ],
        [
            "obfuscated path",
            { _id: "f:0123abcd", type: "plain", path: "/\\:%=AAAA", children: [], ctime: 0, mtime: 0, size: 0 },
            "main",
            { error: "unsupported", unsupported: "PathObfuscation", divergence: "upstream decrypts the /\\: metadata envelope" },
        ],
        [
            "internal file with encrypted metadata",
            { _id: "i:f:0123abcd", type: "newnote", path: "/\\:%=AAAA", children: [], ctime: 0, mtime: 0, size: 0 },
            "main",
            { error: "unsupported", unsupported: "EncryptedMetadata", divergence: "upstream decrypts the /\\: metadata envelope" },
        ],
    ];
    const remoteCases = [];
    for (const [name, doc, key = "main", rust] of remote) {
        for (const accept of [false, true]) {
            const { outgoing: out } = transforms(key, accept);
            const r = await outcome(() => out(structuredClone(doc)));
            const upstreamSide = r.ok ? { ok: r.ok } : { ...classify(r.error), upstream: r.error };
            remoteCases.push({
                name,
                key,
                accept,
                remote: doc,
                outgoing: rust ? { ...rust, upstreamResult: upstreamSide } : upstreamSide,
            });
        }
    }
    return { extra: { keys: KEYS }, cases, remote: remoteCases };
}

// ---------- policy ----------
async function policy() {
    const docs = [
        { _id: "h:+c", type: "leaf", data: "x" },
        { _id: "h:+c", type: "leaf", data: "%=x", e_: true },
        { _id: "h:+c", type: "leaf" },
        { _id: "h:+c", type: "leaf", data: null },
        { _id: "h:+c", _deleted: true, data: "x" },
        { _id: "h:c", type: "leaf", data: "x" },
        { _id: "h:c", type: "leaf", data: "x", e_: true },
        { _id: "x", type: "leaf", data: "x" },
        { _id: "x", type: "chunkpack", data: "x" },
        { _id: "x", type: "chunkpack", data: "x", e_: true },
        { _id: "note.md", type: "plain", children: [] },
        { _id: "note.md", type: "plain", children: [], eden: {} },
        { _id: "note.md", type: "plain", children: [], eden: { "h:+e": { data: "x", epoch: 1 } } },
        { _id: "note.md", type: "plain", children: [], eden: { "h:++encrypted-hkdf": { data: "%=x", epoch: 999999 } } },
        { _id: "note.md", type: "plain", children: [], eden: { "h:++encrypted": { data: "%x", epoch: 999999 } } },
        { _id: "note.md", type: "plain", children: [], eden: { "h:++encrypted-hkdf": { data: "%=x", epoch: 999999 }, "h:+e": {} } },
        { _id: "note.md", type: "plain", children: [], eden: [] },
        { _id: "note.md", type: "plain", children: [], eden: [1] },
        { _id: "note.md", type: "plain", children: [], eden: null },
        { _id: "note.md", type: "plain", children: [], eden: "s" },
        { _id: "note.md", type: "plain", data: "x" },
        { _id: "note.md", type: "plain", data: "x", e_: true },
        { _id: "note.md", type: "plain", data: "x", deleted: true },
        { _id: "note.md", type: "newnote", data: "x", deleted: true },
        { _id: "note.md", type: "internalfile", data: "x", deleted: true },
        { _id: "note.md", type: "notes", data: "x", deleted: true },
        { _id: "note.md", type: "plain", eden: { "h:+e": {} }, deleted: true },
        { _id: "note.md", type: "plain", eden: { "h:+e": {} }, data: "x", e_: true },
        { _id: "note.md", type: "plain", eden: { "h:+e": {} }, _deleted: true },
        { _id: "note.md", data: "x" },
        { _id: "_local/x", data: "x" },
        { _id: "syncinfo", type: "syncinfo", data: "x" },
        { _id: "syncinfo", type: "other", data: "x" },
        { _id: "syncinfo", data: "x", e_: true },
        { _id: "_local/obsidian_livesync_sync_parameters", type: "sync-parameters", pbkdf2salt: "x" },
        { _id: "obsydian_livesync_version", type: "versioninfo", data: "x" },
        { _id: "obsydian_livesync_version", type: "leaf", data: "x" },
        { _id: "f:abc", type: "plain", data: "x" },
        { _id: "f:abc", type: "plain", data: "x", e_: true },
        { _id: "f:abc", type: "plain", children: [] },
        { _id: "i:f:abc", type: "newnote", data: "x" },
    ];
    const cases = [];
    for (const doc of docs) {
        for (const accept of [false, true]) {
            const r = await outcome(() => assertUnencryptedContentPolicy(structuredClone(doc), "v2", accept));
            cases.push({ doc, accept, verdict: r.error ? classify(r.error) : "ok" });
        }
    }
    return { cases };
}

// ---------- sync parameters ----------
async function syncParams() {
    const salt32 = b64(bytes(0xa11, 32));
    const inputs = [
        salt32,
        b64(bytes(0xa12, 16)),
        b64(bytes(0xa13, 1)),
        salt32.replace(/=+$/, ""),
        salt32.slice(0, 20) + "\n" + salt32.slice(20),
        " " + salt32 + "\t",
        "",
        "not*base64",
        "abcde",
        KEYS.main.pbkdf2Salt,
    ];
    const cases = inputs.map((pbkdf2salt) => {
        const decoded = new Uint8Array(base64ToArrayBufferInternalBrowser(pbkdf2salt));
        return {
            pbkdf2salt,
            // Empty salt means upstream generates and stores a new one first.
            upstreamCreatesSalt: !pbkdf2salt,
            decoded: b64(decoded),
            decodedLength: decoded.length,
        };
    });
    return { cases };
}

export async function generateCrypto(emit) {
    setGlobalLogFunction(() => {});
    const u = { upstream };
    const ids = await chunkIds();
    emit("crypto_chunk_ids.json", ids.cases, u);
    const env = await envelopes();
    emit("crypto_envelopes.json", env.cases, { ...u, ...env.extra, failures: env.failures });
    const docs = await documents();
    emit("crypto_documents.json", docs.cases, { ...u, ...docs.extra, remote: docs.remote });
    const pol = await policy();
    emit("crypto_policy.json", pol.cases, u);
    const sp = await syncParams();
    emit("crypto_sync_params.json", sp.cases, u);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
    const outDir = process.argv[2] ?? join(here, "..", "..", "fixtures", "golden");
    mkdirSync(outDir, { recursive: true });
    const meta = { generator: "tools/golden/crypto.mjs", package: pkg.name, version: pkg.version };
    const emit = (name, cases, extra = {}) => {
        writeFileSync(join(outDir, name), JSON.stringify({ meta, ...extra, cases }, null, 1) + "\n");
        console.log(`${name}: ${cases.length} cases`);
    };
    await generateCrypto(emit);
}
