/** Every CR and LF becomes one space — the same collapse `say()` applies in docker-entrypoint.sh. */
export function sanitizeLogLine(line: string): string {
  return line.replace(/[\r\n]/g, " ");
}

/** `[label] msg`, CR/LF-collapsed across the whole composed line. */
export function taggedLine(label: string, msg: string): string {
  return sanitizeLogLine(`[${label}] ${msg}`);
}

/** `bots-app: msg`, CR/LF-collapsed across the whole composed line. */
export function botsAppLine(msg: string): string {
  return sanitizeLogLine(`bots-app: ${msg}`);
}

/** `conduct: msg`, CR/LF-collapsed across the whole composed line. */
export function conductLine(msg: string): string {
  return sanitizeLogLine(`conduct: ${msg}`);
}
