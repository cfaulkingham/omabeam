.pragma library

var MAX_STATUS = 8192
var MAX_TITLE = 200
var MAX_ERROR = 400
var MAX_URL = 256
var MAX_SOURCE = 200
var MAX_PLAIN = 160
var MAX_PID = 2147483647
var MAX_VIEWERS = 4096
var MAX_DIMENSION = 32768
var TOKEN = /^[0-9a-f]{32}$/

function empty() {
  return { state: "idle", pid: 0, title: "", source: "", url: "",
    viewers: null, uptime: null, width: null, height: null, fps: null }
}

function number(value, max) {
  max = max === undefined ? MAX_PID : max
  return typeof value === "number" && isFinite(value) && value >= 0 && value <= max
    ? value : null
}

function plain(value, max) {
  max = max === undefined ? MAX_PLAIN : max
  var text = String(value || "")
  var out = ""
  for (var i = 0; i < text.length && out.length < max; i++) {
    var ch = text.charAt(i)
    var code = text.charCodeAt(i)
    if (ch === "<" || ch === ">" || ch === "&") continue
    if (code < 32 || (code >= 127 && code <= 159)) continue
    if (code === 0x200e || code === 0x200f) continue
    if (code >= 0x202a && code <= 0x202e) continue
    if (code >= 0x2066 && code <= 0x2069) continue
    out += ch
  }
  return out
}

function boundedString(value, max) {
  if (typeof value !== "string") return null
  if (value.length > max) return null
  if (value.indexOf("\0") >= 0) return null
  return value
}

function ipv4Octets(host) {
  var parts = host.split(".")
  if (parts.length !== 4) return null
  var out = []
  for (var i = 0; i < 4; i++) {
    if (!/^[0-9]{1,3}$/.test(parts[i])) return null
    var n = Number(parts[i])
    if (n > 255 || parts[i] !== String(n)) return null
    out.push(n)
  }
  return out
}

function isPrivateIPv4(host) {
  var o = ipv4Octets(host)
  if (!o) return false
  if (o[0] === 127) return true
  if (o[0] === 10) return true
  if (o[0] === 192 && o[1] === 168) return true
  if (o[0] === 172 && o[1] >= 16 && o[1] <= 31) return true
  if (o[0] === 169 && o[1] === 254) return true
  return false
}

function isPrivateIPv6(host) {
  var h = String(host || "").toLowerCase()
  if (!h || /[^0-9a-f:]/.test(h)) return false
  if (h === "::1" || h === "0:0:0:0:0:0:0:1") return true
  if (h.indexOf("fe80:") === 0 || h.indexOf("fe80::") === 0) return true
  if (h.indexOf("fc") === 0 || h.indexOf("fd") === 0) return true
  return false
}

function parseShareUrl(url) {
  var text = String(url || "")
  if (!text || text.length > MAX_URL) return null
  var match = text.match(/^http:\/\/(\[([0-9a-f:]+)\]|((?:[0-9]{1,3}\.){3}[0-9]{1,3})):([1-9][0-9]{0,4})\/s\/([0-9a-f]{32})\/$/i)
  if (!match) return null
  var port = Number(match[4])
  if (port < 1 || port > 65535) return null
  var token = match[5].toLowerCase()
  if (!TOKEN.test(token)) return null
  if (match[2]) {
    if (!isPrivateIPv6(match[2])) return null
    return { host: "[" + match[2] + "]", port: port, token: token }
  }
  if (!isPrivateIPv4(match[3])) return null
  return { host: match[3], port: port, token: token }
}

function linkHost(url) {
  var parsed = parseShareUrl(url)
  if (!parsed) return ""
  return parsed.host + ":" + parsed.port
}

function read(raw, code, exitStatus) {
  var text = String(raw || "")
  if (text.length > MAX_STATUS) throw new Error("OmaBeam returned an unknown status. Try again.")
  text = text.trim()
  if (exitStatus === 0 && code === 1 && text === "") return empty()
  if (exitStatus !== 0 || code !== 0) throw new Error("Could not check the current share. Try again.")
  var data
  try { data = JSON.parse(text) } catch (e) { throw new Error("OmaBeam returned an unreadable status. Try again.") }
  if (!data || typeof data !== "object" || Array.isArray(data)
      || !number(data.pid, MAX_PID) || Math.floor(data.pid) !== data.pid || data.pid < 1
      || (data.state !== "live" && data.state !== "ended"))
    throw new Error("OmaBeam returned an unknown status. Try again.")
  var title = boundedString(data.title, MAX_TITLE)
  var source = data.source == null ? "" : boundedString(data.source, MAX_SOURCE)
  var error = data.error == null ? "" : boundedString(data.error, MAX_ERROR)
  if (title === null || source === null || error === null)
    throw new Error("OmaBeam returned an unknown status. Try again.")
  var url = ""
  if (data.url != null) {
    if (typeof data.url !== "string" || data.url.length > MAX_URL)
      throw new Error("OmaBeam returned an unknown status. Try again.")
    url = parseShareUrl(data.url) ? data.url : ""
  }
  var session = empty()
  session.state = data.state
  session.pid = data.pid
  session.title = title.trim() ? title : "Shared source"
  session.source = source
  session.url = url
  session.error = error
  session.viewers = number(data.viewers, MAX_VIEWERS)
  session.uptime = number(data.uptime, 365 * 24 * 3600)
  session.width = number(data.width, MAX_DIMENSION)
  session.height = number(data.height, MAX_DIMENSION)
  session.fps = number(data.fps, 120)
  return session
}

// Only accept a small monochrome grid for the exact session requested.
function readQr(raw, expectedUrl) {
  if (typeof raw !== "string" || raw.length > MAX_STATUS || !parseShareUrl(expectedUrl)) return []
  var data
  try { data = JSON.parse(raw) } catch (e) { return [] }
  if (!data || data.url !== expectedUrl || !Array.isArray(data.rows)) return []
  var size = data.rows.length
  if (size < 21 || size > 81 || (size - 21) % 4 !== 0) return []
  for (var i = 0; i < size; ++i)
    if (typeof data.rows[i] !== "string" || data.rows[i].length !== size || !/^[01]+$/.test(data.rows[i])) return []
  return data.rows
}

function elapsed(seconds) {
  if (seconds === null) return "—"
  var total = Math.floor(seconds)
  var hours = Math.floor(total / 3600)
  var minutes = Math.floor(total % 3600 / 60)
  var secs = total % 60
  return (hours ? hours + ":" + (minutes < 10 ? "0" : "") : "")
    + minutes + ":" + (secs < 10 ? "0" : "") + secs
}

function quality(session) {
  var values = []
  if (session.width > 0 && session.height > 0) values.push(session.width + " × " + session.height)
  if (session.fps !== null) values.push(session.fps.toFixed(1) + " fps")
  return values.join("  ·  ")
}

function sourceKind(session) {
  var label = session.source || session.title
  if (/^Output /i.test(label)) return "Screen"
  if (/^Region /i.test(label)) return "Area"
  return "Source"
}
