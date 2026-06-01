// Wire.swift — Swift mirror of the daemon's request/reply protocol.
//
// Keep these shapes in lockstep with the Rust side:
//   cli/src/commands/mount/fskit/protocol.rs
//
// Encoding is JSON, framed with a big-endian UInt32 byte length (see
// DaemonClient.swift and ipc.rs). serde's default enum representation is
// "externally tagged": a unit variant serializes as the bare string
// ("Ok"), and a variant with data as {"Variant": <payload>}. The Codable
// conformances below match that exactly.

import Foundation

enum WireKind: UInt8 {
    case dir = 0
    case file = 1
    case symlink = 2
}

/// One VFS operation forwarded to the daemon. Mirrors Rust `enum Op`.
enum Op: Encodable {
    case lookup(parent: UInt64, name: String)
    case getattr(ino: UInt64)
    case readdir(ino: UInt64)
    case read(ino: UInt64, offset: Int64, size: UInt32)
    case write(ino: UInt64, offset: Int64, data: Data)
    case create(parent: UInt64, name: String, exec: Bool)
    case mkdir(parent: UInt64, name: String)
    case unlink(parent: UInt64, name: String)
    case rmdir(parent: UInt64, name: String)
    case rename(parent: UInt64, name: String, newparent: UInt64, newname: String)
    case truncate(ino: UInt64, size: UInt64)

    func encode(to encoder: Encoder) throws {
        // serde externally-tagged: { "Variant": { fields… } }
        var c = encoder.container(keyedBy: GenericKey.self)
        switch self {
        case let .lookup(parent, name):
            try c.encode(LookupP(parent: parent, name: name), forKey: .init("Lookup"))
        case let .getattr(ino):
            try c.encode(InoP(ino: ino), forKey: .init("Getattr"))
        case let .readdir(ino):
            try c.encode(InoP(ino: ino), forKey: .init("Readdir"))
        case let .read(ino, offset, size):
            try c.encode(ReadP(ino: ino, offset: offset, size: size), forKey: .init("Read"))
        case let .write(ino, offset, data):
            // serde expects Vec<u8> as a JSON array of numbers.
            try c.encode(WriteP(ino: ino, offset: offset, data: [UInt8](data)), forKey: .init("Write"))
        case let .create(parent, name, exec):
            try c.encode(CreateP(parent: parent, name: name, exec: exec), forKey: .init("Create"))
        case let .mkdir(parent, name):
            try c.encode(NameP(parent: parent, name: name), forKey: .init("Mkdir"))
        case let .unlink(parent, name):
            try c.encode(NameP(parent: parent, name: name), forKey: .init("Unlink"))
        case let .rmdir(parent, name):
            try c.encode(NameP(parent: parent, name: name), forKey: .init("Rmdir"))
        case let .rename(parent, name, newparent, newname):
            try c.encode(RenameP(parent: parent, name: name, newparent: newparent, newname: newname),
                         forKey: .init("Rename"))
        case let .truncate(ino, size):
            try c.encode(TruncateP(ino: ino, size: size), forKey: .init("Truncate"))
        }
    }

    private struct LookupP: Encodable { let parent: UInt64; let name: String }
    private struct InoP: Encodable { let ino: UInt64 }
    private struct ReadP: Encodable { let ino: UInt64; let offset: Int64; let size: UInt32 }
    private struct WriteP: Encodable { let ino: UInt64; let offset: Int64; let data: [UInt8] }
    private struct CreateP: Encodable { let parent: UInt64; let name: String; let exec: Bool }
    private struct NameP: Encodable { let parent: UInt64; let name: String }
    private struct RenameP: Encodable { let parent: UInt64; let name: String; let newparent: UInt64; let newname: String }
    private struct TruncateP: Encodable { let ino: UInt64; let size: UInt64 }
}

struct AttrWire: Decodable {
    let ino: UInt64
    let size: UInt64
    let kind: UInt8
    let perm: UInt16
    let mtime_ns: UInt128
}

struct DirEntryWire: Decodable {
    let ino: UInt64
    let kind: UInt8
    let name: String
}

/// The daemon's reply. Mirrors Rust `enum Reply`.
enum Reply: Decodable {
    case attr(AttrWire)
    case entries([DirEntryWire])
    case data(Data)
    case written(UInt32)
    case ok
    case errno(Int32)

    init(from decoder: Decoder) throws {
        // serde externally-tagged: "Ok" (bare string) or { "Variant": payload }.
        if let s = try? decoder.singleValueContainer().decode(String.self), s == "Ok" {
            self = .ok
            return
        }
        let c = try decoder.container(keyedBy: GenericKey.self)
        if let k = c.allKeys.first {
            switch k.stringValue {
            case "Attr": self = .attr(try c.decode(AttrWire.self, forKey: k))
            case "Entries": self = .entries(try c.decode([DirEntryWire].self, forKey: k))
            case "Data": self = .data(Data(try c.decode([UInt8].self, forKey: k)))
            case "Written": self = .written(try c.decode(UInt32.self, forKey: k))
            case "Errno": self = .errno(try c.decode(Int32.self, forKey: k))
            default:
                throw DecodingError.dataCorruptedError(
                    forKey: k, in: c, debugDescription: "unknown Reply variant \(k.stringValue)")
            }
            return
        }
        throw DecodingError.dataCorrupted(.init(codingPath: [], debugDescription: "empty Reply"))
    }
}

/// A dynamic CodingKey so we can use the serde variant name as the key.
struct GenericKey: CodingKey {
    var stringValue: String
    var intValue: Int? { nil }
    init(_ s: String) { self.stringValue = s }
    init?(stringValue: String) { self.stringValue = stringValue }
    init?(intValue: Int) { return nil }
}
