#!/usr/bin/env node
// Generates golden decode vectors by running the ORIGINAL ao-loot-logger
// implementation over synthetic Photon packets, then writes them where the
// Rust integration test reads them.
//
//   node tools/gen-golden.js ../ao-loot-logger-main tests/golden/vectors.json
//
// The point is that loot-ledger's decoder is not being compared against my own
// understanding of the protocol — it is being compared against the
// implementation that is known to work against live Albion traffic. If the
// reference and the port disagree, one of them is wrong and the test says so.
//
// Run this only when the reference changes. The output is committed, so a
// normal `cargo test` needs no Node at all.

const fs = require('fs')
const path = require('path')
const Module = require('module')

const [refDir, outPath] = process.argv.slice(2)
if (!refDir || !outPath) {
  console.error('usage: gen-golden.js <ao-loot-logger-dir> <out.json>')
  process.exit(2)
}

// The reference pulls in `winston` purely to write a debug log file, and
// `cap` only for its own capture layer (we feed it bytes directly). Neither is
// needed to decode a packet, and installing them would mean touching the
// reference checkout and reaching the network. Stub them instead: the decoder
// under test is the real one, only its logging is silenced.
const noopLogger = new Proxy(
  {},
  {
    get: () => () => noopLogger,
    apply: () => noopLogger,
  }
)
const winstonStub = {
  createLogger: () => noopLogger,
  format: {
    combine: () => ({}),
    timestamp: () => ({}),
    errors: () => ({}),
    printf: () => ({}),
  },
  transports: { File: class {}, Console: class {} },
}

const originalLoad = Module._load
Module._load = function (request, parent, isMain) {
  if (request === 'winston') return winstonStub
  if (request === 'cap') return { Cap: class {}, decoders: {} }
  return originalLoad.apply(this, arguments)
}

const PhotonParser = require(path.resolve(refDir, 'src/network/photon/photon-parser'))

// --- packet construction -----------------------------------------------------

const DATA_TYPE = {
  NIL: 0x2a, DICTIONARY: 0x44, INT8: 0x62, DOUBLE: 0x64, EVENT_DATE: 0x65,
  FLOAT32: 0x66, INT32: 0x69, INT16: 0x6b, INT64: 0x6c, BOOLEAN: 0x6f,
  STRING: 0x73, INT8_SLICE: 0x78, SLICE: 0x79
}

const COMMAND = {
  ACKNOWLEDGE: 0x01, CONNECT: 0x02, VERIFY_CONNECT: 0x03, DISCONNECT: 0x04,
  PING: 0x05, SEND_RELIABLE: 0x06, SEND_UNRELIABLE: 0x07,
  SEND_RELIABLE_FRAGMENT: 0x08
}

const MSG = {
  OPERATION_REQUEST: 0x02, OPERATION_RESPONSE: 0x03, EVENT_DATA: 0x04,
  INTERNAL_OPERATION_REQUEST: 0x06, INTERNAL_OPERATION_RESPONSE: 0x07
}

/** Photon header: peer id, flags, command count, timestamp, challenge. */
function header(flags, commandCount) {
  const b = Buffer.alloc(12)
  b.writeUInt16BE(0, 0)
  b.writeUInt8(flags, 2)
  b.writeUInt8(commandCount, 3)
  b.writeUInt32BE(0, 4)
  b.writeInt32BE(0, 8)
  return b
}

/** A command: type, channel, flags, reserved, length, sequence. */
function command(type, payload, sequence = 0) {
  const head = Buffer.alloc(12)
  head.writeUInt8(type, 0)
  head.writeUInt8(0, 1)
  head.writeUInt8(0, 2)
  head.writeUInt8(0, 3)
  head.writeInt32BE(payload.length + 12, 4)
  head.writeInt32BE(sequence, 8)
  return Buffer.concat([head, payload])
}

/** Wrap a parameter table in an event payload. */
function eventPayload(code, params) {
  const parts = [Buffer.from([0xf3, MSG.EVENT_DATA, code])]
  parts.push(paramTable(params))
  return Buffer.concat(parts)
}

function opRequestPayload(code, params) {
  return Buffer.concat([
    Buffer.from([0xf3, MSG.OPERATION_REQUEST, code]),
    paramTable(params)
  ])
}

function opResponsePayload(code, returnCode, params) {
  return Buffer.concat([
    Buffer.from([0xf3, MSG.OPERATION_RESPONSE, code]),
    (() => { const b = Buffer.alloc(2); b.writeUInt16BE(returnCode, 0); return b })(),
    Buffer.from([DATA_TYPE.NIL]), // debug message type
    paramTable(params)
  ])
}

function paramTable(params) {
  const encoded = params.map(([id, type, value]) => {
    const head = Buffer.from([id, type])
    return Buffer.concat([head, encodeValue(type, value)])
  })
  const count = Buffer.alloc(2)
  count.writeInt16BE(encoded.length, 0)
  return Buffer.concat([count, ...encoded])
}

function encodeValue(type, value) {
  switch (type) {
    case DATA_TYPE.STRING: {
      const b = Buffer.alloc(2 + Buffer.byteLength(value))
      b.writeUInt16BE(Buffer.byteLength(value), 0)
      b.write(value, 2)
      return b
    }
    case DATA_TYPE.BOOLEAN: return Buffer.from([value ? 1 : 0])
    case DATA_TYPE.INT8: return Buffer.from([value & 0xff])
    case DATA_TYPE.INT16: { const b = Buffer.alloc(2); b.writeUInt16BE(value, 0); return b }
    case DATA_TYPE.INT32: { const b = Buffer.alloc(4); b.writeInt32BE(value, 0); return b }
    case DATA_TYPE.INT64: { const b = Buffer.alloc(8); b.writeBigInt64BE(BigInt(value), 0); return b }
    case DATA_TYPE.FLOAT32: { const b = Buffer.alloc(4); b.writeFloatBE(value, 0); return b }
    case DATA_TYPE.DOUBLE: { const b = Buffer.alloc(8); b.writeDoubleBE(value, 0); return b }
    case DATA_TYPE.EVENT_DATE: { const b = Buffer.alloc(8); b.writeBigInt64BE(BigInt(value), 0); return b }
    case DATA_TYPE.NIL: return Buffer.alloc(0)
    case DATA_TYPE.INT8_SLICE: {
      const b = Buffer.alloc(4 + value.length)
      b.writeUInt32BE(value.length, 0)
      Buffer.from(value).copy(b, 4)
      return b
    }
    case DATA_TYPE.SLICE: {
      const b = Buffer.alloc(3)
      b.writeUInt16BE(value.items.length, 0)
      b.writeUInt8(value.elementType, 2)
      const items = value.items.map(v => encodeValue(value.elementType, v))
      return Buffer.concat([b, ...items])
    }
    case DATA_TYPE.DICTIONARY: {
      const b = Buffer.alloc(4)
      b.writeUInt8(value.keyType, 0)
      b.writeUInt8(value.valueType, 1)
      b.writeUInt16BE(value.entries.length, 2)
      const parts = []
      for (const [k, v] of value.entries) {
        parts.push(encodeValue(value.keyType, k), encodeValue(value.valueType, v))
      }
      return Buffer.concat([b, ...parts])
    }
    default: throw new Error(`gen-golden: no encoder for type 0x${type.toString(16)}`)
  }
}

/** A SEND_RELIABLE_FRAGMENT command body. */
function fragment(seq, count, number, total, offset, data) {
  const b = Buffer.alloc(20)
  b.writeInt32BE(seq, 0)
  b.writeInt32BE(count, 4)
  b.writeInt32BE(number, 8)
  b.writeInt32BE(total, 12)
  b.writeInt32BE(offset, 16)
  return Buffer.concat([b, data])
}

// --- normalising decoded values ---------------------------------------------
//
// The reference returns plain JavaScript values and discards the wire type, so
// the Rust side is normalised the same way: numbers, strings, booleans, arrays
// and plain objects. Dictionary keys are stringified on both sides.

function normalise(v) {
  if (v === null || v === undefined) return null
  if (typeof v === 'bigint') return Number(v)
  if (Array.isArray(v)) return v.map(normalise)
  if (typeof v === 'object') {
    const out = {}
    for (const k of Object.keys(v).sort()) out[String(k)] = normalise(v[k])
    return out
  }
  return v
}

/** Run one packet through the reference parser, collecting what it emits. */
function decode(packet) {
  const parser = new PhotonParser()
  const out = []
  parser.on('event-data', (e) => out.push({ kind: 'event', code: e.eventCode, params: normalise(e.parameters) }))
  parser.on('request-data', (o) => out.push({ kind: 'request', code: o.operationCode, params: normalise(o.parameters) }))
  parser.on('response-data', (o) => out.push({ kind: 'response', code: o.operationCode, returnCode: o.returnCode, params: normalise(o.parameters) }))
  try {
    parser.handlePhotonPacket(packet)
  } catch (e) {
    return { emitted: out, threw: String(e && e.message) }
  }
  return { emitted: out, threw: null }
}

// --- the vectors ------------------------------------------------------------

const cases = []

/** Record a case decoded by a fresh parser. */
function add(name, packet) {
  cases.push({ name, payload: packet.toString('hex'), ...decode(packet) })
}

/**
 * Record a case built from several packets fed to ONE parser, which is what
 * fragment reassembly requires.
 */
function addSequence(name, packets) {
  const parser = new PhotonParser()
  const out = []
  parser.on('event-data', (e) => out.push({ kind: 'event', code: e.eventCode, params: normalise(e.parameters) }))
  parser.on('request-data', (o) => out.push({ kind: 'request', code: o.operationCode, params: normalise(o.parameters) }))
  parser.on('response-data', (o) => out.push({ kind: 'response', code: o.operationCode, returnCode: o.returnCode, params: normalise(o.parameters) }))
  let threw = null
  for (const p of packets) {
    try {
      parser.handlePhotonPacket(p)
    } catch (e) {
      threw = String(e && e.message)
      break
    }
  }
  cases.push({
    name,
    packets: packets.map((p) => p.toString('hex')),
    emitted: out,
    threw
  })
}

add(
  'loot-event-mixed-types',
  Buffer.concat([
    header(0x04, 1),
    command(COMMAND.SEND_RELIABLE, eventPayload(1, [
      [1, DATA_TYPE.STRING, 'BossRat'],
      [2, DATA_TYPE.STRING, 'Grim'],
      [3, DATA_TYPE.BOOLEAN, false],
      [4, DATA_TYPE.INT32, 1234],
      [5, DATA_TYPE.INT32, 3],
      [252, DATA_TYPE.INT32, 275]
    ]))
  ])
)

add(
  'event-with-unicode-names',
  Buffer.concat([
    header(0x04, 1),
    command(COMMAND.SEND_RELIABLE, eventPayload(1, [
      [1, DATA_TYPE.STRING, 'Grüße 日本語'],
      [2, DATA_TYPE.STRING, 'Ünïcödé'],
      [51, DATA_TYPE.STRING, 'Caoimhe'],
      [252, DATA_TYPE.INT32, 29]
    ]))
  ])
)

add(
  'event-with-int64-and-float',
  Buffer.concat([
    header(0x04, 1),
    command(COMMAND.SEND_RELIABLE, eventPayload(1, [
      [7, DATA_TYPE.INT64, 9007199254740],
      [8, DATA_TYPE.FLOAT32, 1.5],
      [10, DATA_TYPE.INT16, 1234],
      [11, DATA_TYPE.INT8, 42],
      [12, DATA_TYPE.NIL, null],
      [252, DATA_TYPE.INT32, 275]
    ]))
  ])
)

add(
  'event-with-slice',
  Buffer.concat([
    header(0x04, 1),
    command(COMMAND.SEND_RELIABLE, eventPayload(1, [
      [20, DATA_TYPE.SLICE, { elementType: DATA_TYPE.INT32, items: [10, 20, 30] }],
      [252, DATA_TYPE.INT32, 275]
    ]))
  ])
)

add(
  'event-with-dictionary',
  Buffer.concat([
    header(0x04, 1),
    command(COMMAND.SEND_RELIABLE, eventPayload(1, [
      [30, DATA_TYPE.DICTIONARY, {
        keyType: DATA_TYPE.STRING,
        valueType: DATA_TYPE.INT32,
        entries: [['alpha', 1], ['beta', 2]]
      }],
      [252, DATA_TYPE.INT32, 275]
    ]))
  ])
)

add(
  'event-with-empty-string-and-zero-quantity',
  Buffer.concat([
    header(0x04, 1),
    command(COMMAND.SEND_RELIABLE, eventPayload(1, [
      [1, DATA_TYPE.STRING, ''],
      [2, DATA_TYPE.STRING, 'Grim'],
      [3, DATA_TYPE.BOOLEAN, true],
      [5, DATA_TYPE.INT32, 0],
      [252, DATA_TYPE.INT32, 275]
    ]))
  ])
)

add(
  'operation-request',
  Buffer.concat([
    header(0x04, 1),
    command(COMMAND.SEND_RELIABLE, opRequestPayload(29, [
      [0, DATA_TYPE.INT32, 7],
      [253, DATA_TYPE.INT32, 29]
    ]))
  ])
)

add(
  'operation-response',
  Buffer.concat([
    header(0x04, 1),
    command(COMMAND.SEND_RELIABLE, opResponsePayload(2, 0, [
      [2, DATA_TYPE.STRING, 'Grim'],
      [57, DATA_TYPE.STRING, 'The Vanguished'],
      [77, DATA_TYPE.STRING, 'Caoimhe'],
      [253, DATA_TYPE.INT32, 2]
    ]))
  ])
)

add(
  'two-commands-in-one-packet',
  Buffer.concat([
    header(0x04, 2),
    command(COMMAND.SEND_RELIABLE, eventPayload(1, [
      [2, DATA_TYPE.STRING, 'First'],
      [252, DATA_TYPE.INT32, 275]
    ]), 1),
    command(COMMAND.SEND_RELIABLE, eventPayload(1, [
      [2, DATA_TYPE.STRING, 'Second'],
      [252, DATA_TYPE.INT32, 275]
    ]), 2)
  ])
)

add(
  'unreliable-command-with-sequence-prefix',
  Buffer.concat([
    header(0x04, 1),
    command(
      COMMAND.SEND_UNRELIABLE,
      Buffer.concat([
        Buffer.from([0, 0, 0, 42]), // reliable sequence prefix, not part of the message
        eventPayload(1, [[2, DATA_TYPE.STRING, 'Unreliable'], [252, DATA_TYPE.INT32, 275]])
      ])
    )
  ])
)

// Two fragments of one message, delivered out of order.
{
  const whole = eventPayload(1, [
    [1, DATA_TYPE.STRING, 'FragmentedVictim'],
    [2, DATA_TYPE.STRING, 'FragmentedLooter'],
    [4, DATA_TYPE.INT32, 987],
    [5, DATA_TYPE.INT32, 11],
    [252, DATA_TYPE.INT32, 275]
  ])
  const cut = Math.floor(whole.length / 2)
  const head = whole.subarray(0, cut)
  const tail = whole.subarray(cut)

  // A lone fragment must emit nothing.
  add('fragment-incomplete', Buffer.concat([
    header(0x04, 1),
    command(COMMAND.SEND_RELIABLE_FRAGMENT, fragment(7, 2, 0, whole.length, 0, head), 7)
  ]))

  // Out-of-order delivery, then the head: the message must emerge intact.
  addSequence('fragmented-event-reassembled-out-of-order', [
    Buffer.concat([
      header(0x04, 1),
      command(COMMAND.SEND_RELIABLE_FRAGMENT, fragment(7, 2, 1, whole.length, cut, tail), 7)
    ]),
    Buffer.concat([
      header(0x04, 1),
      command(COMMAND.SEND_RELIABLE_FRAGMENT, fragment(7, 2, 0, whole.length, 0, head), 7)
    ])
  ])
}

add('encrypted-packet', Buffer.concat([
  header(0x01, 1),
  command(COMMAND.SEND_RELIABLE, eventPayload(1, [[252, DATA_TYPE.INT32, 275]]))
]))

add('ping-only-packet', Buffer.concat([
  header(0x04, 1),
  command(COMMAND.PING, Buffer.from([0xaa, 0xbb, 0xcc, 0xdd]), 1)
]))

add('unknown-command-type-is-skipped', Buffer.concat([
  header(0x04, 2),
  command(0x42, Buffer.from([1, 2, 3, 4]), 1),
  command(COMMAND.SEND_RELIABLE, eventPayload(1, [
    [2, DATA_TYPE.STRING, 'AfterUnknown'],
    [252, DATA_TYPE.INT32, 275]
  ]), 2)
]))

add('truncated-packet', Buffer.from([0x00, 0x00, 0x04, 0x01, 0x00]))
add('empty-packet', Buffer.alloc(0))

add('event-with-unknown-param-type', Buffer.concat([
  header(0x04, 1),
  command(COMMAND.SEND_RELIABLE, Buffer.concat([
    Buffer.from([0xf3, MSG.EVENT_DATA, 1]),
    (() => { const b = Buffer.alloc(2); b.writeInt16BE(1, 0); return b })(),
    Buffer.from([252, 0x63, 0xff, 0xff, 0xff]) // CUSTOM: not modelled
  ]))
]))

add('event-with-zero-parameter-count', Buffer.concat([
  header(0x04, 1),
  command(COMMAND.SEND_RELIABLE, Buffer.concat([
    Buffer.from([0xf3, MSG.EVENT_DATA, 1]),
    Buffer.from([0x00, 0x00])
  ]))
]))

add('event-with-negative-parameter-count', Buffer.concat([
  header(0x04, 1),
  command(COMMAND.SEND_RELIABLE, Buffer.concat([
    Buffer.from([0xf3, MSG.EVENT_DATA, 1]),
    Buffer.from([0xff, 0xff])
  ]))
]))

// --- output ------------------------------------------------------------------

const doc = {
  description:
    'Golden decode vectors produced by the reference ao-loot-logger implementation. ' +
    'Regenerate with: node tools/gen-golden.js ../ao-loot-logger-main tests/golden/vectors.json',
  cases
}

fs.mkdirSync(path.dirname(outPath), { recursive: true })
fs.writeFileSync(outPath, JSON.stringify(doc, null, 2) + '\n')

const totalEmitted = cases.reduce((n, c) => n + c.emitted.length, 0)
console.log(`wrote ${cases.length} case(s), ${totalEmitted} emitted message(s) -> ${outPath}`)
