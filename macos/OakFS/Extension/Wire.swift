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

/// Version of the daemon wire protocol this extension speaks. Must match
/// `PROTOCOL_VERSION` in cli/src/commands/mount/fskit/protocol.rs. The extension
/// announces it via `Op.hello` at mount time (see `DaemonClient.handshake`) and
/// refuses to mount if the daemon reports a different one — an `Oak Mount` app
/// and `oak` CLI from different releases would otherwise trade misinterpreted
/// frames. Bump in lockstep with the Rust `PROTOCOL_VERSION`.
///
/// History: v1 → v2 added `Op.readlink` (symlink target resolution).
let kOakProtocolVersion: UInt32 = 2

enum WireKind: UInt8 {
    case dir = 0
    case file = 1
    case symlink = 2
}

/// One VFS operation forwarded to the daemon. Mirrors Rust `enum Op`.
enum Op: Encodable {
    case hello(protocolVersion: UInt32)
    case lookup(parent: UInt64, name: String)
    case getattr(ino: UInt64)
    case readdir(ino: UInt64)
    case read(ino: UInt64, offset: Int64, size: UInt32)
    case readlink(ino: UInt64)
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
        case let .hello(protocolVersion):
            try c.encode(HelloP(protocol_version: protocolVersion), forKey: .init("Hello"))
        case let .lookup(parent, name):
            try c.encode(LookupP(parent: parent, name: name), forKey: .init("Lookup"))
        case let .getattr(ino):
            try c.encode(InoP(ino: ino), forKey: .init("Getattr"))
        case let .readdir(ino):
            try c.encode(InoP(ino: ino), forKey: .init("Readdir"))
        case let .read(ino, offset, size):
            try c.encode(ReadP(ino: ino, offset: offset, size: size), forKey: .init("Read"))
        case let .readlink(ino):
            try c.encode(InoP(ino: ino), forKey: .init("Readlink"))
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

    private struct HelloP: Encodable { let protocol_version: UInt32 }
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

/// Payload of `Reply::Hello { protocol_version }`.
private struct HelloReply: Decodable { let protocol_version: UInt32 }

/// The daemon's reply. Mirrors Rust `enum Reply`.
enum Reply: Decodable {
    case hello(protocolVersion: UInt32)
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
            case "Hello": self = .hello(protocolVersion: try c.decode(HelloReply.self, forKey: k).protocol_version)
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
