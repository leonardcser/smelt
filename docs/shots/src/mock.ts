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

/** A file too long for the confirm dialog to show at its default height. */
const discountModule = `//! Discount rules, split out of \`pricing\`.

/// How a discount is taken off a price.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// A percentage off the price.
    Percent(u32),
    /// A fixed amount off, in cents.
    Fixed(u32),
}

impl Rule {
    /// Price in cents after the rule is applied. Never goes below zero.
    pub fn apply(self, price_cents: u32) -> u32 {
        match self {
            Rule::Percent(percent) => {
                let percent = percent.min(100);
                price_cents - price_cents * percent / 100
            }
            Rule::Fixed(cents) => price_cents.saturating_sub(cents),
        }
    }
}
`;

type Message = { role: string; content?: unknown };

const asText = (content: unknown) => (typeof content === "string" ? content : JSON.stringify(content ?? ""));

/** Scripted answers for the stills (a plain question, an approval, a goal), the theme replays and the vim demo's long answer. */
function reply(messages: Message[]): Chunk[] {
  const lastUser = messages.findLastIndex((m) => m.role === "user");
  const user = asText(messages[lastUser]?.content);
  const system = asText(messages.find((m) => m.role === "system")?.content);

  if (/title/i.test(system) && messages.length <= 3) return text("Fix failing cart tests");
  if (/discount\.rs/i.test(user)) {
    return calls("I'll move the discount rules into their own file.", [
      { name: "write_file", args: { file_path: "/tmp/acme-app/src/discount.rs", content: discountModule } },
    ]);
  }
  if (/run the tests/i.test(user)) return bash("I'll run the test suite first.", "cargo test", "Run the test suite");
  if (/rounding/i.test(user)) {
    return text(
      "## The fix\n\n`apply_discount` divides by `10` instead of `100`, so a 10% discount takes off the whole price:\n\n```rust\npub fn apply_discount(price_cents: u32, percent: u32) -> u32 {\n    let percent = percent.min(100); // never more than the price\n    price_cents - price_cents * percent / 100\n}\n```\n\n- **Clamp** the percentage so a discount above 100% cannot underflow\n- **Divide by 100**, which makes `discount_applies_to_total` pass\n- Run `cargo test` to confirm\n",
    );
  }
  if (/quick tour/i.test(user)) {
    return text(
      "## Project tour\n\n`acme-app` is a small Rust crate that prices a shopping cart.\n\n### Source\n\n- `src/lib.rs` declares the two public modules, `cart` and `pricing`.\n- `src/pricing.rs` holds `apply_discount`, which takes a price in cents and a percentage and returns the discounted price.\n- `src/cart.rs` defines `Item` and `Cart`. `Cart::total` calls `apply_discount` for every line item and sums the results.\n\n### Tests\n\n- `tests/cart.rs` covers an empty cart, a cart with no discount, a 10% discount and a 100% discount.\n\n### How the pieces fit\n\nA `Cart` owns a list of `Item`s. Each item carries its own `discount_percent`, so the discount is applied per item before the totals are summed. Rounding happens in integer cents inside `apply_discount`, the only place the discount math lives.\n\n### Things to watch\n\n- The percentage is not clamped, so a discount above 100% underflows.\n- `apply_discount` divides by 10 instead of 100.\n- Nothing tests rounding at the edges.\n",
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
