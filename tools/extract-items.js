#!/usr/bin/env node
// Converts the item database shipped with ao-loot-logger into the plain TSV that
// loot-ledger embeds. Run once; the output is committed so the Rust build needs no
// network access.
//
//   node tools/extract-items.js ../ao-loot-logger-main/src/items-fallback.js src/assets/items.tsv

const fs = require('fs')
const path = require('path')

const [input, output] = process.argv.slice(2)

if (!input || !output) {
  console.error('usage: extract-items.js <items-fallback.js> <out.tsv>')
  process.exit(2)
}

const raw = require(path.resolve(input))

const rows = []
const seen = new Set()

for (const line of raw.trim().split('\n')) {
  // Format is: "<numId>: <UNIQUE_NAME> : <Display Name>"
  const firstSep = line.indexOf(':')
  if (firstSep === -1) continue

  const numId = parseInt(line.slice(0, firstSep).trim(), 10)
  if (!Number.isFinite(numId)) continue

  const rest = line.slice(firstSep + 1)
  const secondSep = rest.indexOf(':')

  const unique = (secondSep === -1 ? rest : rest.slice(0, secondSep)).trim()
  const display = secondSep === -1 ? unique : rest.slice(secondSep + 1).trim()

  if (seen.has(numId)) continue
  seen.add(numId)

  rows.push({ numId, unique, display: display || unique })
}

rows.sort((a, b) => a.numId - b.numId)

fs.writeFileSync(
  path.resolve(output),
  rows.map((r) => `${r.numId}\t${r.unique}\t${r.display}`).join('\n') + '\n'
)

console.log(`wrote ${rows.length} items to ${output}`)
