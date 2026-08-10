# Design

lkv uses one binary image containing an immutable hash table and an append-only update log.
The format is designed for zero-copy point reads, atomic batch updates, and simple crash recovery.

All integers are unsigned and little-endian. Checksums detect accidental corruption;
they do not authenticate the file against intentional modification.

## File layout

The first 8 KiB are reserved for two Superblocks. All Base and Overlay data follows them.

```text
0x0000  Superblock 0                 4 KiB
0x1000  Superblock 1                 4 KiB
0x2000  Base and Overlay generations
```

A generation consists of one immutable Base followed by zero or more Overlay records.
Compaction temporarily appends a new generation before relocating it to the start of the data area.

```text
older --------------------------------------------------------------> newer

Base 0 | Overlay 0 | compact marker | staging Base
  ^                                      ^
  +-- old Superblock                     +-- staging Superblock
```

Only the generation selected by the active Superblock is visible.
Older bytes are inert. A completed compaction leaves one Base at offset 8192 and truncates everything after it.

This append-first layout keeps published data immutable. 
A new generation becomes visible through one small metadata switch after its Base has reached stable storage.

## Superblocks

Each Superblock occupies 4096 bytes. Its 72-byte header is:

| Offset | Size | Field                                     |
| -----: | ---: | ----------------------------------------- |
|      0 |    8 | magic: `LKV\0\0\0\0\0`                   |
|      8 |    4 | format version                            |
|     12 |    4 | header size: `72`                         |
|     16 |    8 | generation                                |
|     24 |    8 | Base offset                               |
|     32 |    8 | Base size                                 |
|     40 |    8 | Base slot count                           |
|     48 |    8 | Base entry count                          |
|     56 |    8 | Overlay offset                            |
|     64 |    4 | CRC32C of the Base checksum metadata      |
|     68 |    4 | CRC32C of bytes 0..68                     |
|     72 | 4024 | unused padding, written as zero           |

The current format version is 1. Base and Overlay offsets are absolute file offsets. Base size
includes its checksum table and footer.

The first 24 bytes and the final CRC32C position form a version-independent envelope.
Future versions retain the magic, version, header size, and generation offsets, and store the
header CRC32C in the final four bytes of the declared header. This lets an older implementation
reject a valid newer Superblock instead of opening and truncating an older generation.

Generation parity selects the Superblock slot. Readers choose the valid Superblock with the highest generation. 
Alternating slots preserve the previous root if writing the next root is interrupted.
The generation number prevents slot order from being mistaken for recency.

The final CRC32C detects torn or damaged root metadata, including the Base metadata checksum.
The Base metadata checksum binds the selected Base to its checksum table and footer.

## Base segment

The Base is a frozen open-addressing hash table:

```text
Base header | slot table | packed records | block CRC32Cs | checksum footer
```

### Header

| Offset | Size | Field              |
| -----: | ---: | ------------------ |
|      0 |    4 | section id: `HASH` |
|      4 |    4 | header size: `24`  |
|      8 |    8 | slot count         |
|     16 |    8 | entry count        |

The slot count is zero for an empty Base. Otherwise it is a power of two sized for a load factor no higher than 80%.

### Slot table

Each slot is 12 bytes:

| Offset | Size | Field                  |
| -----: | ---: | ---------------------- |
|      0 |    4 | key fingerprint        |
|      4 |    8 | absolute record offset |

An empty slot contains two zero fields. The initial slot is the low bits of XXH3-64.
Collisions use linear probing. The fingerprint is the upper 32 bits of the same hash,
but a match is always confirmed against the complete key.

The compact slot representation gives point reads one predictable probe sequence. 
Absolute record offsets allow a slot to address packed key and value bytes directly.

### Records

Records immediately follow the slot table. They have no padding:

```text
key length:u32 | value length:u32 | key bytes | value bytes
```

Keys are limited to 1 MiB. Key and value lengths must fit in `u32`.

Packing records minimizes file size and lets mapped values be returned without decoding or copying.
The tradeoff is that the Base cannot be updated in place; it must be rebuilt as a unit.

### Checksums

Base data means the header, slot table, and records. It is divided into 64 KiB blocks. One CRC32C is
stored for each block immediately after the data.

The segment ends with this 24-byte footer:

| Offset | Size | Field                        |
| -----: | ---: | ---------------------------- |
|      0 |    4 | section id: `CRC3`           |
|      4 |    4 | footer size: `24`            |
|      8 |    8 | Base data size               |
|     16 |    4 | checksum block size: `65536` |
|     20 |    4 | CRC32C of the checksum table |

The footer locates the checksum table from the end of the segment. The table CRC protects checksum metadata.
The Superblock also stores a CRC32C of the complete checksum table and footer.

Per-block checksums allow touched data to be verified lazily. 
Opening the file therefore need not scan a large Base, while full verification can still cover every byte.

## Overlay log

The Overlay is a sequence of self-contained records. Each record starts with a 17-byte header:

| Offset | Size | Field                   |
| -----: | ---: | ----------------------- |
|      0 |    1 | record marker           |
|      1 |    4 | key length              |
|      5 |    4 | value or payload length |
|      9 |    4 | record CRC32C           |
|     13 |    4 | header CRC32C           |

The header CRC covers bytes 0..13, including the record CRC. It is checked before the lengths are trusted.
The record CRC covers the marker, both lengths, and the complete payload.

Protecting the header separately prevents damaged lengths from turning corruption into an unbounded read.
Covering the header fields again in the record CRC binds the payload to its type and declared size.

### Batch record

A batch has marker `4`, key length `0`, and its payload length in the value-length field. 
The payload is:

```text
operation count:u32
repeat operation count times:
    operation:u8 | key length:u32 | value length:u32 | key bytes | value bytes
```

Operation `1` stores a value. Operation `2` is a deletion and must have a zero value length.
At most 1,000,000 operations are accepted in one batch, and the complete payload must fit in `u32`.

One batch is one atomic update. A complete batch is either accepted in full or rejected in full;
individual operations are never recovered independently.

### Compact marker

A compact marker has marker `3`, both lengths set to zero, and no payload. It terminates the active Overlay.

The marker is written before a new Base. If a crash occurs before the new Superblock is published,
the old generation remains active and recovery stops at the marker instead of interpreting the new Base as log data.

Markers `1` and `2` are valid only inside a batch. Standalone mutations are not part of the format.

## Publication and recovery

A batch is appended and synchronized before it is considered visible.
An incomplete final header or payload is an interrupted append and may be discarded. 
A complete record with an invalid checksum or structure is corruption and is not silently repaired.

A compacted Base is published in this order:

```text
append compact marker and a staging Base
synchronize and publish the staging Base
rebuild the Base at offset 8192
append a compact marker after the relocated Base
synchronize the relocated Base and marker
publish the relocated Base in both Superblock pages
release the old mapping and truncate after the relocated Base
synchronize and remap the compacted image
```

Before the first Superblock switch, the old generation is authoritative. The staging generation remains
authoritative while the front of the file is rewritten. The final marker prevents recovery from interpreting
untruncated staging bytes as Overlay records if compaction stops after the relocated Base is published.

Every published transition points to a complete synchronized Base. After success, the file contains only the two
Superblock pages and one live Base. Compaction uses no sidecar file, but temporarily grows the database to hold a
staging Base and writes the live data twice. Snapshots must be dropped before compaction so the old mapping can be
released before truncation.
