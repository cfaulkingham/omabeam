.pragma library

function empty() {
  return { state: "idle", pid: 0, title: "", source: "", url: "",
    viewers: null, uptime: null, width: null, height: null, fps: null }
}

function number(value) {
  return typeof value === "number" && isFinite(value) && value >= 0 ? value : null
}

function linkHost(url) {
  // Only browser URLs are actionable. Never hand an arbitrary URI scheme to Qt.
  var match = String(url || "").match(/^https?:\/\/(\[[0-9a-f:.]+\]|[a-z0-9.-]+)(?::([0-9]{1,5}))?(\/[^\s\\]*)?$/i)
  if (!match || (match[2] && (Number(match[2]) < 1 || Number(match[2]) > 65535))) return ""
  return match[1] + (match[2] ? ":" + match[2] : "")
}

function read(raw, code, exitStatus) {
  var text = String(raw || "").trim()
  if (exitStatus === 0 && code === 1 && text === "") return empty()
  if (exitStatus !== 0 || code !== 0) throw new Error("Could not check the current share. Try again.")
  var data
  try { data = JSON.parse(text) } catch (e) { throw new Error("OmaBeam returned an unreadable status. Try again.") }
  if (!data || typeof data !== "object" || Array.isArray(data)
      || !number(data.pid) || Math.floor(data.pid) !== data.pid
      || (data.state !== "live" && data.state !== "ended"))
    throw new Error("OmaBeam returned an unknown status. Try again.")
  var session = empty()
  session.state = data.state
  session.pid = data.pid
  session.title = typeof data.title === "string" && data.title.trim() ? data.title : "Shared source"
  session.source = typeof data.source === "string" ? data.source : ""
  session.url = typeof data.url === "string" && linkHost(data.url) ? data.url : ""
  session.error = typeof data.error === "string" ? data.error : ""
  session.viewers = number(data.viewers)
  session.uptime = number(data.uptime)
  session.width = number(data.width)
  session.height = number(data.height)
  session.fps = number(data.fps)
  return session
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
