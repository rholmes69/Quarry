//! Compiles SQLite3 Multiple Ciphers (SQLite 3.53 + encryption codecs) and
//! links it statically into Quarry.
//!
//! SQLite3MC is the continuation of wxSQLite3's encryption extension by the
//! same author. One library reads and writes:
//!   * ChaCha20-Poly1305 (sqleet)              — default for new databases
//!   * AES-128-CBC / AES-256-CBC (wxSQLite3)
//!   * SQLCipher v1, v2, v3 and v4
//!   * RC4 (System.Data.SQLite)
//!   * Ascon-128 and AEGIS
//!
//! rusqlite is built with its `in_gecko` feature, which means "SQLite is
//! linked in by the embedding application": libsqlite3-sys then emits no link
//! instructions of its own, so this is the only SQLite in the binary.

fn main() {
    let src = sqlite3mc_src::source_dir();
    let file = src.join(sqlite3mc_src::SOURCE_FILE);
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", file.display());

    let target = std::env::var("TARGET").unwrap_or_default();
    let windows = target.contains("windows");

    let mut b = cc::Build::new();
    b.file(&file).include(src).warnings(false);
    for (k, v) in [
        // same feature set rusqlite's own bundled build uses
        ("SQLITE_CORE", None),
        ("SQLITE_DEFAULT_FOREIGN_KEYS", Some("1")),
        ("SQLITE_ENABLE_API_ARMOR", None),
        ("SQLITE_ENABLE_COLUMN_METADATA", None),
        ("SQLITE_ENABLE_DBSTAT_VTAB", None),
        ("SQLITE_ENABLE_FTS3", None),
        ("SQLITE_ENABLE_FTS3_PARENTHESIS", None),
        ("SQLITE_ENABLE_FTS4", None),
        ("SQLITE_ENABLE_FTS5", None),
        ("SQLITE_ENABLE_JSON1", None),
        ("SQLITE_ENABLE_LOAD_EXTENSION", Some("1")),
        ("SQLITE_ENABLE_MEMORY_MANAGEMENT", None),
        ("SQLITE_ENABLE_RTREE", None),
        ("SQLITE_ENABLE_GEOPOLY", None),
        ("SQLITE_ENABLE_STAT4", None),
        ("SQLITE_ENABLE_EXPLAIN_COMMENTS", None),
        ("SQLITE_SOUNDEX", None),
        ("SQLITE_THREADSAFE", Some("1")),
        ("SQLITE_USE_URI", Some("1")),
        ("SQLITE_DQS", Some("0")),
        ("HAVE_USLEEP", Some("1")),
        ("HAVE_ISNAN", None),
        ("_POSIX_THREAD_SAFE_FUNCTIONS", None),
        // SQLite3MC extras that need no external libraries
        ("SQLITE_ENABLE_REGEXP", Some("1")),
        ("SQLITE_ENABLE_SERIES", Some("1")),
        ("SQLITE_ENABLE_UUID", Some("1")),
        ("SQLITE_ENABLE_SHA3", Some("1")),
        ("SQLITE_ENABLE_CARRAY", Some("1")),
        ("SQLITE_ENABLE_CSV", Some("1")),
        // math & string helpers (sqrt, power, stdev, padl, proper, ...)
        ("SQLITE_ENABLE_EXTFUNC", Some("1")),
        // compress/sqlar/zipfile (need zlib) and fileio (touches the file
        // system from SQL) are deliberately left out.
        // Encryption only covers database/journal/WAL files; keep SQLite's
        // temporary tables and sort spills in memory so no plaintext ever
        // lands in a temp file.
        ("SQLITE_TEMP_STORE", Some("2")),
        ("SQLITE_SECURE_DELETE", Some("1")),
    ] {
        b.define(k, v);
    }
    if !windows {
        b.define("HAVE_LOCALTIME_R", None);
    }
    b.opt_level(2);
    // Produces libsqlite3mc.a / sqlite3mc.lib and asks rustc to link it. One
    // object file, linked whole so link order can never drop it.
    b.cargo_metadata(false);
    b.compile("sqlite3mc");
    let out = std::env::var("OUT_DIR").unwrap();
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static:+whole-archive=sqlite3mc");
    if windows {
        // RAND_s / CryptGenRandom for IVs and salts
        println!("cargo:rustc-link-lib=advapi32");
    }
    println!("cargo:rustc-env=QUARRY_SQLITE3MC_VERSION={}", sqlite3mc_src::SQLITE3MC_VERSION);
}
