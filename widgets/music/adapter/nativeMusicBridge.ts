/** Playback adapter for the Rust widget. No Electron and no extra music page. */
import fs from 'node:fs'
import path from 'node:path'
import readline from 'node:readline'
import { createHash } from 'node:crypto'
import { MusicPlayer } from './musicPlayer'
import { createMusicWindowHost } from './musicPlayerWindow'

const root = process.cwd()
const directory = path.join(root, 'config', 'native-music-player')
const cache = path.join(directory, 'art')
fs.mkdirSync(cache, { recursive: true })
const emit = (value: unknown) => process.stdout.write(JSON.stringify(value) + '\n')
const artwork = new Map<string, string>()
const pending = new Set<string>()
const MAX_ART_FILES = 10
let stopping = false

function pruneArtCache(): void {
  try {
    const entries = fs.readdirSync(cache, { withFileTypes: true })
      .filter(entry => entry.isFile() && /^[a-f0-9]{64}\.img$/i.test(entry.name))
      .map(entry => {
        const full = path.join(cache, entry.name)
        return { name: entry.name, full, mtime: fs.statSync(full).mtimeMs }
      })
      .sort((a, b) => b.mtime - a.mtime)
    for (const stale of entries.slice(MAX_ART_FILES)) {
      fs.unlinkSync(stale.full)
      for (const [url, file] of artwork) {
        if (file === stale.name) artwork.delete(url)
      }
    }
  } catch { /* best-effort cleanup */ }
}

// Only fetch artwork URLs supplied by the owned page, with bounded downloads.
async function art(url: string): Promise<void> {
  if (!url || artwork.has(url) || pending.has(url) || pending.size >= 6) return
  let target: URL
  try { target = new URL(url) } catch { return }
  if (target.protocol !== 'https:' || !/(^|\.)(ytimg\.com|googleusercontent\.com|ggpht\.com)$/.test(target.hostname)) return
  pending.add(url)
  try {
    const name = createHash('sha256').update(url).digest('hex') + '.img'
    const destination = path.join(cache, name)
    if (fs.existsSync(destination)) {
      try {
        const now = new Date()
        fs.utimesSync?.(destination, now, now)
      } catch { /* ignore in mocked fs */ }
    } else {
      const response = await fetch(target, { redirect: 'error', headers: { Accept: 'image/jpeg,image/png' }, signal: AbortSignal.timeout(8000) })
      if (!response.ok || !response.headers.get('content-type')?.startsWith('image/')) return
      let size = 0
      const chunks: Uint8Array[] = []
      for await (const chunk of response.body!) {
        size += chunk.length
        if (size > 4 * 1024 * 1024) { await response.body?.cancel().catch(() => {}); return }
        chunks.push(chunk)
      }
      fs.writeFileSync(destination + '.tmp', Buffer.concat(chunks))
      fs.renameSync(destination + '.tmp', destination)
      pruneArtCache()
    }
    artwork.set(url, name)
    if (!stopping) emit({ type: 'art', url, file: name })
  } catch { /* Artwork is optional; playback remains available. */ }
  finally { pending.delete(url) }
}
const player = new MusicPlayer(directory, (state, error) => {
  if (stopping) return
  emit({ type: 'state', state, error: error || null })
  if (state) {
    const currentIndex = state.queue.findIndex(item => item.videoId === state.videoId)
    const selected = currentIndex >= 0 ? currentIndex : state.queue.findIndex(item => item.selected)
    const visible = selected < 0 ? [] : state.queue.slice(Math.max(0, selected - 2), selected + 3)
    for (const url of [state.art, ...visible.map(item => item.thumb), ...state.queue.slice(0, 10).map(item => item.thumb)]) if (url) void art(url)
  }
}, (pid) => createMusicWindowHost(pid))
let chain = Promise.resolve()
async function command(message: { action: string; value?: number; videoId?: string }) {
  if (stopping) return
  console.error(`[music-bridge] action=${message.action}`)
  switch (message.action) {
    case 'browse': await player.open(); break
    case 'library': await player.open('https://music.youtube.com/library'); break
    case 'signin': await player.signIn(); break
    case 'signout': await player.signOut(); break
    case 'layout': break // Chrome owns its client size; never resize a cross-process child.
    case 'select': await player.play(message.videoId || ''); break
    case 'info': emit({ type: 'info', ...await player.instanceInfo() }); break
    case 'toggle': case 'play': case 'pause': case 'next': case 'previous': case 'seek':
      await player.command(message.action, message.value); break
    default: throw new Error('Unknown native music action')
  }
}
const input = readline.createInterface({ input: process.stdin, terminal: false })
input.on('line', line => {
  if (line.length > 4096) return
  let message: { action: string }
  try { message = JSON.parse(line) } catch { return }
  if (message.action === 'quit') { shutdown(); return }
  chain = chain.then(() => command(message)).catch(error => {
    const msg = String(error?.message || error)
    console.error(`[music-bridge] error: ${msg}`)
    if (!stopping) emit({ type: 'error', error: msg })
  })
})
function shutdown() {
  if (stopping) return
  stopping = true
  console.error('[music-bridge] shutting down')
  player.stop()
  input.close()
  setTimeout(() => process.exit(0), 1500).unref()
}
input.on('close', shutdown)
process.on('SIGTERM', shutdown)
emit({ type: 'ready' })
