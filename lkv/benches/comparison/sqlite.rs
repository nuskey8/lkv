use crate::common::{
    BATCH_SIZE, INITIAL_ENTRIES, directory_size, key, mib, read_indices, temp_dir, value,
};
use ::rusqlite::{Connection, params};
use test::{Bencher, black_box};

fn create(path: &std::path::Path) -> Connection {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "PRAGMA synchronous = FULL;
         CREATE TABLE kv (key BLOB PRIMARY KEY, value BLOB NOT NULL);",
        )
        .unwrap();
    connection
}

fn populate(connection: &Connection) {
    let transaction = connection.unchecked_transaction().unwrap();
    {
        let mut statement = transaction
            .prepare("INSERT INTO kv(key, value) VALUES (?1, ?2)")
            .unwrap();
        for index in 0..INITIAL_ENTRIES {
            statement
                .execute(params![key(index).as_slice(), value(index, 0).as_slice()])
                .unwrap();
        }
    }
    transaction.commit().unwrap();
}

fn populated() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
    let dir = temp_dir();
    let path = dir.path().join("db.sqlite");
    let connection = create(&path);
    populate(&connection);
    (dir, path, connection)
}

#[bench]
fn bulk_load_100k(b: &mut Bencher) {
    let mut dirs = Vec::new();
    b.iter(|| {
        let dir = temp_dir();
        let path = dir.path().join("db.sqlite");
        let connection = create(&path);
        populate(&connection);
        drop(connection);
        dirs.push(dir);
    });
    black_box(dirs);
}

#[bench]
fn write_single_sync(b: &mut Bencher) {
    let (_dir, _path, connection) = populated();
    let mut index = INITIAL_ENTRIES;
    b.iter(|| {
        let transaction = connection.unchecked_transaction().unwrap();
        transaction
            .execute(
                "INSERT INTO kv(key, value) VALUES (?1, ?2)",
                params![key(index).as_slice(), value(index, 1).as_slice()],
            )
            .unwrap();
        transaction.commit().unwrap();
        index += 1;
    });
}

#[bench]
fn write_batch_1000_sync(b: &mut Bencher) {
    let (_dir, _path, connection) = populated();
    let mut next = INITIAL_ENTRIES;
    b.iter(|| {
        let transaction = connection.unchecked_transaction().unwrap();
        {
            let mut statement = transaction
                .prepare("INSERT INTO kv(key, value) VALUES (?1, ?2)")
                .unwrap();
            for index in next..next + BATCH_SIZE {
                statement
                    .execute(params![key(index).as_slice(), value(index, 1).as_slice()])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
        next += BATCH_SIZE;
    });
}

#[bench]
fn read_random_100k(b: &mut Bencher) {
    let (_dir, _path, connection) = populated();
    let indices = read_indices();
    let mut statement = connection
        .prepare("SELECT value FROM kv WHERE key = ?1")
        .unwrap();
    b.iter(|| {
        let checksum = indices.iter().fold(0u64, |sum, &index| {
            let first: u8 = statement
                .query_row([key(index).as_slice()], |row| {
                    let value = row.get_ref(0)?.as_blob()?;
                    Ok(value[0])
                })
                .unwrap();
            sum.wrapping_add(u64::from(first))
        });
        black_box(checksum)
    });
}

#[bench]
fn delete_single_sync(b: &mut Bencher) {
    let (_dir, _path, connection) = populated();
    let mut index = 0;
    b.iter(|| {
        assert!(index < INITIAL_ENTRIES, "delete fixture exhausted");
        let transaction = connection.unchecked_transaction().unwrap();
        assert_eq!(
            transaction
                .execute("DELETE FROM kv WHERE key = ?1", [key(index).as_slice()])
                .unwrap(),
            1
        );
        transaction.commit().unwrap();
        index += 1;
    });
}

#[bench]
fn size_uncompacted_compacted(b: &mut Bencher) {
    let (dir, _path, connection) = populated();
    let transaction = connection.unchecked_transaction().unwrap();
    {
        let mut update = transaction
            .prepare("UPDATE kv SET value = ?2 WHERE key = ?1")
            .unwrap();
        for index in 0..INITIAL_ENTRIES {
            update
                .execute(params![key(index).as_slice(), value(index, 1).as_slice()])
                .unwrap();
        }
        let mut delete = transaction
            .prepare("DELETE FROM kv WHERE key = ?1")
            .unwrap();
        for index in 0..INITIAL_ENTRIES / 2 {
            delete.execute([key(index).as_slice()]).unwrap();
        }
    }
    transaction.commit().unwrap();
    let uncompacted = directory_size(dir.path());
    connection.execute("VACUUM", []).unwrap();
    let compacted = directory_size(dir.path());
    eprintln!(
        "SIZE sqlite uncompacted={:.2}MiB compacted={:.2}MiB",
        mib(uncompacted),
        mib(compacted)
    );
    b.iter(|| black_box((uncompacted, compacted)));
}
