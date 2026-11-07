use crate::text::width;
use crate::{render, Error};

const FLOW_TD: &str = r#"graph TD
  Go["Go []; &"] --> Check{"x < 3?"}
  Check -->|"y|n"| Reserve
  Check -- no --> Waitlist
  Reserve --> Paid{Paid?}
  Waitlist --> Notify[Notify buyer]
  Paid -- yes --> Ship
  Paid -- no --> Retry[Retry payment]
  Retry --> Paid
  Ship --> Notify"#;

const FLOW_LR: &str = "flowchart LR
  A[Request] --> B{Valid?}
  B -- yes --> C[Process]
  B -- no --> D[Reject]
  C --> E[(Database)]
  C --> F[Notify user]
  D --> F
  F --> G([End])";

const FLOW_RL: &str = r#"flowchart RL
  A(["请求<br>Ready"]) -->|go| B[[Work next]]
  B --> B
  B -.->|no| C{"Done?<br/>Retry?"}
  C ==> A
  A <--> C"#;

const FLOW_BT: &str = "flowchart BT
  a[Ship 🚀] --> b[/Réponse?/]
  b --> c((请求))
  c --x a
  c --o d>Flag]";

const FLOW_FANOUT: &str = "flowchart TD
  A --> B & C & D
  B --> E
  C --> E
  D --> E
  A --> E
  E --> F
  B --> F
  X[Lonely]
  F -- loop back --> F";

const SEQUENCE: &str = r#"sequenceDiagram
    autonumber
    actor Buyer
    participant API
    participant Inv as 库存
    participant Pay as Payments
    Buyer->>+API: data[0] = {x: 1}
    API->>Inv: Reserve items
    Inv-->>API: say "hello" | C:\tmp
    opt Items reserved
      loop Up to 3 attempts
        API->>Pay: Charge card
        Pay->>Pay: Check fraud
        Pay--)API: Payment status
      end
      alt Approved
        Note over API,Inv: Payment recorded
        API-->>-Buyer: Order confirmed
      else Declined
        API-xInv: Release items
        Note right of Pay: no retry
        API-->>Buyer: Payment failed
      end
    end"#;

const STATE: &str = r#"stateDiagram-v2
    [*] --> Draft
    state "Order drafted" as Draft
    Draft : Awaiting submission
    Draft --> Validating : submit
    Validating --> Pending : [ready]
    Validating --> Rejected : invalid
    state Pending {
        [*] --> Charging
        Charging --> Charging : retry
        Charging --> [*]
    }
    note right of Pending : not drawn
    Pending --> Packing : paid
    Pending --> Rejected : declined
    Rejected --> Draft : revise
    Packing --> Shipped : dispatch
    Shipped --> [*]"#;

const CLASS: &str = "classDiagram
    class Order {
        +int[] ids
        +String status
        +submit()
        +cancel() bool
    }
    class LineItem {
        +int quantity
        +subtotal() Decimal
    }
    class Payment {
        <<abstract>>
        +Decimal amount
        +authorize()*
    }
    CardPayment : +String lastFour
    Order \"1\" *-- \"1..*\" LineItem : contains
    Order --> Payment : pays with
    Payment <|-- CardPayment
    Repo~T~ ..|> Payment";

const ER: &str = r#"---
title: Shop
---
erDiagram
    CUSTOMER ||--o{ ORDER : places
    CUSTOMER {
        int id PK
        string email UK
    }
    ORDER ||--|{ LINE_ITEM : contains
    LINE_ITEM {
        int order_id PK, FK "order key"
        int quantity
    }
    PRODUCT ||..o{ LINE_ITEM : "appears in""#;

const PIE: &str = "pie showData title Pets adopted
    \"Dogs\" : 386
    \"Cats\" : 85.5
    \"Rats\" : 15";

const MINDMAP: &str = "mindmap
  root((Mermaid))
    Origins
      Long history
      ::icon(fa fa-book)
      Popularisation<br>British author
    Research
      On effectiveness
    Tools[Tools]";

const ALL: &[(&str, &str)] = &[
    ("flowchart_td", FLOW_TD),
    ("flowchart_lr", FLOW_LR),
    ("flowchart_rl", FLOW_RL),
    ("flowchart_bt", FLOW_BT),
    ("flowchart_fanout", FLOW_FANOUT),
    ("sequence", SEQUENCE),
    ("state", STATE),
    ("class", CLASS),
    ("er", ER),
    ("pie", PIE),
    ("mindmap", MINDMAP),
];

#[test]
fn renders_every_family() {
    for (name, source) in ALL {
        let out = render(source, 120).unwrap_or_else(|e| panic!("{name}: {e}"));
        insta::assert_snapshot!(*name, out);
    }
}

#[test]
fn output_never_exceeds_width() {
    for (name, source) in ALL {
        for max in [12, 24, 40, 60, 80, 120] {
            match render(source, max) {
                Ok(out) => {
                    let widest = out.lines().map(width).max().unwrap_or(0);
                    assert!(widest <= max, "{name} at {max}: {widest} columns\n{out}");
                }
                Err(Error::TooWide) => {}
                Err(e) => panic!("{name} at {max}: {e}"),
            }
        }
    }
}

#[test]
fn narrow_width_turns_horizontal_flowchart_vertical() {
    let out = render(FLOW_LR, 40).unwrap();
    insta::assert_snapshot!(out);
}

#[test]
fn every_prefix_renders_or_fails_cleanly() {
    for (_, source) in ALL {
        let mut cut = String::new();
        for c in source.chars() {
            cut.push(c);
            let _ = render(&cut, 80);
        }
    }
}

#[test]
fn rejects_unknown_and_malformed_sources() {
    assert_eq!(
        render("gantt\n  title Plan", 80),
        Err(Error::Unsupported("gantt".into()))
    );
    assert!(matches!(render("", 80), Err(Error::Syntax(_))));
    assert!(matches!(
        render("flowchart TD\n  A --> ", 80),
        Err(Error::Syntax(_))
    ));
    assert!(matches!(
        render("sequenceDiagram\n  loop x\n  A->>B: hi", 80),
        Err(Error::Syntax(_))
    ));
    assert!(matches!(
        render("flowchart XY\n  A --> B", 80),
        Err(Error::Syntax(_))
    ));
}

#[test]
fn rejects_oversized_graphs() {
    let mut source = String::from("flowchart TD\n");
    for i in 0..300 {
        source.push_str(&format!("  n{i} --> n{}\n", i + 1));
    }
    assert_eq!(render(&source, 200), Err(Error::TooLarge));
}

#[test]
fn edges_never_share_a_line() {
    // Two edges converge on and diverge from the same nodes; each must keep
    // its own port, so no junction glyph joins different edges.
    let out = render(
        "flowchart TD\n  A --> C\n  B --> C\n  C --> D\n  C --> E",
        80,
    )
    .unwrap();
    insta::assert_snapshot!(out);
}
