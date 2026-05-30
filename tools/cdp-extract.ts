// Pull the assistant report text out of a running Chrome tab over CDP.
// Node/tsx only (raw WebSocket to DevTools); closes the socket in finally.
// Usage: tsx tools/cdp-extract.ts <cdpPort> <urlSubstring> > out.md

const port = process.argv[2] ?? "64907";
const urlNeedle = process.argv[3] ?? "chatgpt.com/c/";

type Target = { type: string; url: string; webSocketDebuggerUrl?: string };

async function main() {
  const targets: Target[] = await fetch(`http://127.0.0.1:${port}/json`).then((r) => r.json());
  const tab = targets.find((t) => t.type === "page" && t.url.includes(urlNeedle) && t.webSocketDebuggerUrl);
  if (!tab?.webSocketDebuggerUrl) throw new Error(`no page tab matching "${urlNeedle}" on :${port}`);

  const ws = new WebSocket(tab.webSocketDebuggerUrl);
  try {
    await new Promise<void>((res, rej) => {
      ws.addEventListener("open", () => res(), { once: true });
      ws.addEventListener("error", () => rej(new Error("ws error")), { once: true });
    });

    // Concatenate every assistant turn's visible text (deep-research reports
    // included), falling back to the whole document if the selector misses.
    const expression = `(() => {
      const nodes = [...document.querySelectorAll('[data-message-author-role="assistant"]')];
      const txt = nodes.map(n => n.innerText).join('\\n\\n---\\n\\n').trim();
      return txt.length > 40 ? txt : document.body.innerText;
    })()`;

    const result: string = await new Promise((resolve, reject) => {
      const id = 1;
      const onMsg = (ev: MessageEvent) => {
        const msg = JSON.parse(ev.data as string);
        if (msg.id !== id) return;
        ws.removeEventListener("message", onMsg);
        if (msg.error) return reject(new Error(JSON.stringify(msg.error)));
        resolve(msg.result?.result?.value ?? "");
      };
      ws.addEventListener("message", onMsg);
      ws.send(JSON.stringify({
        id,
        method: "Runtime.evaluate",
        params: { expression, returnByValue: true },
      }));
    });

    process.stdout.write(result);
  } finally {
    ws.close();
  }
}

main().catch((e) => {
  console.error(String(e));
  process.exit(1);
});
