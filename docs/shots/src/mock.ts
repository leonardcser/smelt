// Scripted OpenAI-compatible chat server so the static screenshots never depend on a real provider.
import { PROJECT } from "./paths";

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

/** Tool calls answered so far, so every call gets its own id across a multi-step turn. */
let callIds = 0;

const calls = (content: string, list: { name: string; args: Record<string, unknown> }[]): Chunk[] => [
  chunk({
    role: "assistant",
    content,
    tool_calls: list.map((c, index) => ({
      index,
      id: `call_shots_${callIds++}`,
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

/** The gallery's exchanges, a step for each tool result since the user's message: the lamp fix reads
 * the pricing, fixes the divisor, runs the tests and reports; the crate question looks up two crates;
 * a thanks gets a word back. */
function gallery(user: string, step: number): Chunk[] | null {
  if (/^\W*thanks!\W*$/i.test(user)) return text("Anytime.");
  if (/how does the discount math work\?/i.test(user))
    return text(
      "`apply_discount` takes a price in cents and a percent off. It works out the cut as `price * percent / 100`, " +
        "then takes that from the price, so 10% off a $50 lamp leaves $45.\n\n" +
        "Everything stays in whole cents, so no float rounding creeps in, and a percent over 100 is held at 100 " +
        "so a price never goes below zero. A fixed amount off goes through `Rule::Fixed`, which saturates the same way.",
    );
  if (/equations for gradient descent and adam/i.test(user))
    return text(
      "Gradient descent\n\n$$\\theta_{t+1} = \\theta_t - \\eta\\,\\nabla L(\\theta_t)$$\n\nAdam\n\n" +
        "$$m_t = \\beta_1 m_{t-1} + (1-\\beta_1)\\,g_t,\\qquad v_t = \\beta_2 v_{t-1} + (1-\\beta_2)\\,g_t^2$$\n\n" +
        "$$\\theta_{t+1} = \\theta_t - \\eta\\,\\frac{\\hat m_t}{\\sqrt{\\hat v_t}+\\epsilon}$$\n\nOperations on $g_t$ are elementwise.",
    );
  if (/diagram of how a price is discounted/i.test(user))
    return text(
      "Here is the flow:\n\n```mermaid\nflowchart LR\n  A{Over 100%?} -- yes --> B[Cap at 100%]\n  A -- no --> C[Take % off]\n  B --> C\n  C --> D[New price]\n```\n\n" +
        "A percent over 100% is capped at 100%, so a price never goes below zero.",
    );
  if (/what does apply_discount do\?/i.test(user)) return text("Takes a price in cents and a percent, and returns the price after the discount.");
  if (/money math/i.test(user)) {
    if (step === 0) return calls("", [{ name: "crate_info", args: { name: "rust_decimal" } }, { name: "crate_info", args: { name: "bigdecimal" } }]);
    return text("Use `rust_decimal`: fixed precision, fast, and made for money. `bigdecimal` is for arbitrary precision.");
  }
  if (/plan a test for 0% off/i.test(user)) {
    const plan = (...status: string[]) => ({
      items: ["Read how apply_discount rounds", "Write a test for 0% off", "Run the tests"].map((text, i) => ({ text, status: status[i] })),
    });
    if (step === 0) return calls("", [{ name: "todo", args: plan("active", "todo", "todo") }]);
    if (step === 1) return calls("", [{ name: "todo", args: plan("done", "active", "todo") }]);
    if (step === 2) return calls("", [{ name: "todo", args: plan("done", "done", "active") }]);
    return text("All three done: 0% off leaves the price as it was.");
  }
  if (/how big is each file\?/i.test(user)) {
    if (step === 0) {
      return calls("", [
        { name: "chart", args: { title: "lines per file", bars: [["src/cart.rs", 64], ["src/pricing.rs", 12], ["src/lib.rs", 2], ["tests/cart.rs", 41]].map(([label, value]) => ({ label, value })) } },
      ]);
    }
    return text("`cart.rs` is the biggest; `lib.rs` only declares the modules.");
  }
  if (/plot of the lamp discount/i.test(user)) {
    if (step === 0) return calls("", [{ name: "show_image", args: { file_path: `${PROJECT}/plots/lamp.png` } }]);
    return text("The `/ 10` bug charges nothing for a 10% discount on a $50 lamp.");
  }
  if (/run the checkout tests/i.test(user)) {
    if (step === 0) return bash("", "cargo test -q --test checkout", "Run the checkout tests");
    return text("There is no `checkout` test target. The cart tests live in `tests/cart.rs`.");
  }
  if (!/lamp free/i.test(user)) return null;
  if (step === 0) return calls("", [{ name: "read_file", args: { file_path: `${PROJECT}/src/pricing.rs` } }]);
  if (step === 1) {
    return calls("It divides by `10`, not `100`.", [
      { name: "edit_file", args: { file_path: `${PROJECT}/src/pricing.rs`, old_string: "price_cents * percent / 10\n", new_string: "price_cents * percent / 100\n" } },
    ]);
  }
  if (step === 2) return bash("", "cargo test -q --test cart", "Run the tests");
  return text("Fixed: 10% off a $50 lamp is now $45. All 4 tests pass.");
}

type Message = { role: string; content?: unknown };

const asText = (content: unknown) => (typeof content === "string" ? content : JSON.stringify(content ?? ""));

/** Scripted answers for the stills (a plain question, an approval, a goal), the theme replays and the vim demo's long answer. */
function reply(messages: Message[]): Chunk[] {
  const lastUser = messages.findLastIndex((m) => m.role === "user");
  const user = asText(messages[lastUser]?.content);
  const system = asText(messages.find((m) => m.role === "system")?.content);

  if (/title/i.test(system) && messages.length <= 3) return text("Fix failing cart tests");
  const scripted = gallery(user, messages.slice(lastUser + 1).filter((m) => m.role === "tool").length);
  if (scripted) return scripted;
  if (/discount\.rs/i.test(user)) {
    return calls("I'll move the discount rules into their own file.", [
      { name: "write_file", args: { file_path: `${PROJECT}/src/discount.rs`, content: discountModule } },
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
