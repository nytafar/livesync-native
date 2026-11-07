// Plugin `data.json` and official-CLI `settings.json` files, as upstream's own
// SettingService saves them, for `lsync import` (crates/lsync/tests/import.rs).
//
// Every value is synthetic. The files are what `saveSettingData()` writes:
// the connection moved into `encryptedCouchDBConnection`, remote profiles
// encrypted, the E2EE passphrase in `encryptedPassphrase`, all with the
// configuration passphrase (`*` when `configPassphraseStore` is "", as
// upstream's default). Random salts and IVs come from a seeded generator so
// the output is stable.
import { DEFAULT_SETTINGS, CURRENT_SETTING_VERSION, createNewVaultSettings } from "@vrtmrz/livesync-commonlib/settings";
import { SALT_OF_PASSPHRASE, REMOTE_COUCHDB } from "@vrtmrz/livesync-commonlib/compat/common/types";
import { SettingService } from "@vrtmrz/livesync-commonlib/compat/services/base/SettingService";
import { ServiceContext } from "@vrtmrz/livesync-commonlib/compat/services/base/ServiceBase";
import { decryptString } from "@vrtmrz/livesync-commonlib/compat/encryption/stringEncryption";

const CONFIG_KEY = "ls-setting-passphrase";

// Deterministic randomness for WebCrypto's salts and IVs, only while the
// files are made.
function seededRandom() {
    let seed = 0x2545f491;
    return (arr) => {
        const u8 = new Uint8Array(arr.buffer, arr.byteOffset, arr.byteLength);
        for (let i = 0; i < u8.length; i++) {
            seed ^= seed << 13;
            seed >>>= 0;
            seed ^= seed >>> 17;
            seed ^= seed << 5;
            seed >>>= 0;
            u8[i] = seed & 0xff;
        }
        return arr;
    };
}

class CapturingSettingService extends SettingService {
    constructor(localPassphrase) {
        const values = new Map();
        const deviceValues = {
            getValue: async (k) => values.get(k) ?? null,
            setValue: async (k, v) => void values.set(k, v),
            delete: async (k) => void values.delete(k),
        };
        super(new ServiceContext(), {
            APIService: {
                getSystemVaultName: () => "synthetic-vault",
                getAppID: () => "synthetic-app",
                confirm: { askString: async () => false },
                addLog: () => {},
            },
            deviceValues,
        });
        this.localPassphrase = localPassphrase;
    }
    async saveData(settings) {
        this.saved = structuredClone(settings);
    }
    async loadData() {
        return this.saved && structuredClone(this.saved);
    }
}

const connection = {
    couchDB_URI: "https://couch.example.invalid",
    couchDB_USER: "synthetic-user",
    couchDB_PASSWORD: "synthetic-password-1",
    couchDB_DBNAME: "notes-db",
};
const profileUri = (c) =>
    `sls+https://${encodeURIComponent(c.couchDB_USER)}:${encodeURIComponent(c.couchDB_PASSWORD)}@${new URL(c.couchDB_URI).host}/?db=${c.couchDB_DBNAME}`;

async function save(base, overrides, localPassphrase) {
    const s = new CapturingSettingService(localPassphrase);
    if (localPassphrase !== undefined) {
        await s.deviceValues.setValue(CONFIG_KEY, localPassphrase);
    }
    s.settings = {
        ...structuredClone(base),
        settingVersion: CURRENT_SETTING_VERSION,
        isConfigured: true,
        remoteType: REMOTE_COUCHDB,
        ...connection,
        ...overrides,
    };
    await s.saveSettingData();
    return s.saved;
}

// What each file must decrypt to, checked here with upstream's own decryption.
async function check(name, saved, passphrase) {
    if (saved.encryptedCouchDBConnection) {
        const c = JSON.parse(await decryptString(saved.encryptedCouchDBConnection, passphrase + SALT_OF_PASSPHRASE));
        if (c.couchDB_PASSWORD !== connection.couchDB_PASSWORD) {
            throw new Error(`${name}: the connection does not decrypt`);
        }
    }
}

export async function generateSettings() {
    const had = Object.getOwnPropertyDescriptor(globalThis.crypto, "getRandomValues");
    globalThis.crypto.getRandomValues = seededRandom();
    try {
        return await settingsFiles();
    } finally {
        if (had) Object.defineProperty(globalThis.crypto, "getRandomValues", had);
        else delete globalThis.crypto.getRandomValues;
    }
}

async function settingsFiles() {
    const profiles = (c) => ({
        "remote-synthetic": { id: "remote-synthetic", name: "CouchDB synthetic", uri: profileUri(c), isEncrypted: false },
    });
    const files = {};
    // The plugin, as a vault set up with today's defaults saves it.
    files["plugin-default"] = await save(DEFAULT_SETTINGS, {
        syncIgnoreRegEx: "\\/\\.git\\/|[]|^Archive\\/",
        syncOnlyRegEx: "",
        useIgnoreFiles: true,
        ignoreFiles: ".gitignore, .notesignore",
        remoteConfigurations: profiles(connection),
        activeConfigurationId: "remote-synthetic",
    });
    // The active profile wins over the connection fields, as on upstream's load.
    const other = { ...connection, couchDB_DBNAME: "profile-db", couchDB_PASSWORD: "synthetic-password-2" };
    files["plugin-profile-wins"] = await save(DEFAULT_SETTINGS, {
        remoteConfigurations: profiles(other),
        activeConfigurationId: "remote-synthetic",
    });
    // A configuration passphrase kept on the device (LOCALSTORAGE).
    files["plugin-config-passphrase"] = await save(
        DEFAULT_SETTINGS,
        { configPassphraseStore: "LOCALSTORAGE", remoteConfigurations: {}, activeConfigurationId: "" },
        "synthetic-config-passphrase"
    );
    // End-to-end encryption on.
    files["plugin-e2ee"] = await save(DEFAULT_SETTINGS, {
        encrypt: true,
        passphrase: "synthetic-e2ee-passphrase",
        remoteConfigurations: {},
        activeConfigurationId: "",
    });
    // The official CLI's settings.json (createDefaultCliSettings, then configured).
    files["cli-settings"] = await save(
        { ...createNewVaultSettings(), useIndexedDBAdapter: false },
        { remoteConfigurations: profiles(connection), activeConfigurationId: "remote-synthetic" }
    );
    // An older plugin that kept the connection in plain fields.
    files["plugin-plain"] = {
        ...structuredClone(DEFAULT_SETTINGS),
        settingVersion: CURRENT_SETTING_VERSION,
        isConfigured: true,
        ...connection,
        remoteConfigurations: {},
        activeConfigurationId: "",
        encryptedCouchDBConnection: "",
    };
    await check("plugin-default", files["plugin-default"], "*");
    await check("plugin-config-passphrase", files["plugin-config-passphrase"], "synthetic-config-passphrase");
    return {
        extra: {
            salt_of_passphrase: SALT_OF_PASSPHRASE,
            connection,
            profile_wins: { database: other.couchDB_DBNAME, password: other.couchDB_PASSWORD },
            config_passphrase: "synthetic-config-passphrase",
        },
        files,
    };
}
