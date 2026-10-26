// Scripted OpenAI-compatible chat server so the static screenshots never depend on a real provider.
export const MODEL = "gpt-6-luna";

type Chunk = Record<string, unknown>;

const chunk = (delta: Chunk, finish: string | null = null, extra: Chunk = {}): Chunk => ({
  id: "chatcmpl-shots",
  object: "chat.completion.chunk",
  created: 0,
  model: MODEL,
  choices: [{ index: 0, delta, finish_reason: finish }],
  ...extra,
});

const usage = (completion: number) => ({
  prompt_tokens: 18400,
  completion_tokens: completion,
  total_tokens: 18400 + completion,
});

const text = (t: string): Chunk[] => {
  const words = t.match(/\S+\s*/g) ?? [t];
  return [
    chunk({ role: "assistant", content: "" }),
    ...words.map((w) => chunk({ content: w })),
    chunk({}, "stop", { usage: usage(words.length) }),
  ];
};

const calls = (content: string, list: { name: string; args: Record<string, unknown> }[]): Chunk[] => [
  chunk({
    role: "assistant",
    content,
    tool_calls: list.map((c, index) => ({
      index,
      id: `call_shots_${index}`,
      type: "function",
      function: { name: c.name, arguments: JSON.stringify(c.args) },
    })),
  }),
  chunk({}, "tool_calls", { usage: usage(40) }),
];

const bash = (content: string, command: string, description: string) =>
  calls(content, [{ name: "bash", args: { command, description } }]);

type Message = { role: string; content?: unknown };

const asText = (content: unknown) => (typeof content === "string" ? content : JSON.stringify(content ?? ""));

/** Scripted answers for the stills (a plain question, an approval, a goal) and the theme replays. */
function reply(messages: Message[]): Chunk[] {
  const lastUser = messages.findLastIndex((m) => m.role === "user");
  const user = asText(messages[lastUser]?.content);
  const system = asText(messages.find((m) => m.role === "system")?.content);

  if (/title/i.test(system) && messages.length <= 3) return text("Fix failing cart tests");
  if (/run the tests/i.test(user)) return bash("I'll run the test suite first.", "cargo test", "Run the test suite");
  if (/rounding/i.test(user)) {
    return text(
      "## The fix\n\n`apply_discount` divides by `10` instead of `100`, so a 10% discount takes off the whole price:\n\n```rust\npub fn apply_discount(price_cents: u32, percent: u32) -> u32 {\n    let percent = percent.min(100); // never more than the price\n    price_cents - price_cents * percent / 100\n}\n```\n\n- **Clamp** the percentage so a discount above 100% cannot underflow\n- **Divide by 100**, which makes `discount_applies_to_total` pass\n- Run `cargo test` to confirm\n",
    );
  }
  if (/goal/i.test(user)) {
    return text("Starting on the migration. First I'll list every package that still uses the old config format.");
  }
  return text(
    "`apply_discount` in `src/pricing.rs` takes a price in cents and a percentage and returns the discounted price in cents. `Cart::total` in `src/cart.rs` calls it for every line item and sums the results.",
  );
}

/** Starts the server on an ephemeral port. `wordDelayMs` paces streaming for animated captures. */
export function startMock(wordDelayMs: number) {
  return Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    async fetch(req) {
      if (req.method === "GET") return Response.json({ data: [{ id: MODEL }] });
      const body = (await req.json().catch(() => ({}))) as { messages?: Message[] };
      const chunks = reply(body.messages ?? []);
      const enc = new TextEncoder();
      const stream = new ReadableStream({
        async start(controller) {
          for (const c of chunks) {
            controller.enqueue(enc.encode(`data: ${JSON.stringify(c)}\n\n`));
            if (wordDelayMs) await Bun.sleep(wordDelayMs);
          }
          controller.enqueue(enc.encode("data: [DONE]\n\n"));
          controller.close();
        },
      });
      return new Response(stream, { headers: { "content-type": "text/event-stream" } });
    },
  });
}
