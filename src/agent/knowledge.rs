#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentDocId {
    Overview,
    ConfigSemantics,
    RuntimeGeneration,
    MihomoConfigSpec,
    Dns,
    Tun,
    SystemProxy,
    PortProxy,
    Subscriptions,
    LlmProviders,
    Troubleshooting,
    PatchRules,
}

#[derive(Debug)]
pub struct AgentDoc {
    pub id: AgentDocId,
    pub title: &'static str,
    pub triggers: &'static [&'static str],
    pub priority: u8,
    pub body: &'static str,
}

pub static AGENT_DOCS: &[AgentDoc] = &[
    AgentDoc {
        id: AgentDocId::Overview,
        title: "clashtui overview",
        triggers: &["clashtui", "overview", "architecture"],
        priority: 100,
        body: include_str!("../../doc/agent/overview.md"),
    },
    AgentDoc {
        id: AgentDocId::ConfigSemantics,
        title: "config semantics",
        triggers: &["config", "save", "restart", "draft"],
        priority: 95,
        body: include_str!("../../doc/agent/config-semantics.md"),
    },
    AgentDoc {
        id: AgentDocId::PatchRules,
        title: "patch rules",
        triggers: &["patch", "change", "modify", "set", "add"],
        priority: 90,
        body: include_str!("../../doc/agent/patch-rules.md"),
    },
    AgentDoc {
        id: AgentDocId::RuntimeGeneration,
        title: "runtime generation",
        triggers: &[
            "runtime",
            "generated",
            "mihomo-run",
            "mihomo-active",
            "listener",
        ],
        priority: 70,
        body: include_str!("../../doc/agent/runtime-generation.md"),
    },
    AgentDoc {
        id: AgentDocId::MihomoConfigSpec,
        title: "mihomo config subset",
        triggers: &["mihomo", "mixed-port", "listeners", "proxy-groups", "rules"],
        priority: 65,
        body: include_str!("../../doc/agent/mihomo-config-spec.md"),
    },
    AgentDoc {
        id: AgentDocId::Dns,
        title: "DNS",
        triggers: &[
            "dns",
            "nameserver",
            "nameserver-policy",
            "fake-ip",
            "redir",
            "taobao",
            "direct-nameserver",
        ],
        priority: 85,
        body: include_str!("../../doc/agent/dns.md"),
    },
    AgentDoc {
        id: AgentDocId::Tun,
        title: "TUN",
        triggers: &["tun", "route", "utun", "auto-route", "transparent"],
        priority: 85,
        body: include_str!("../../doc/agent/tun.md"),
    },
    AgentDoc {
        id: AgentDocId::SystemProxy,
        title: "system proxy",
        triggers: &["system proxy", "os proxy", "bypass", "pac"],
        priority: 75,
        body: include_str!("../../doc/agent/system-proxy.md"),
    },
    AgentDoc {
        id: AgentDocId::PortProxy,
        title: "port proxy",
        triggers: &["port proxy", "socks", "http", "mixed", "listener", "7081"],
        priority: 80,
        body: include_str!("../../doc/agent/port-proxy.md"),
    },
    AgentDoc {
        id: AgentDocId::Subscriptions,
        title: "subscriptions",
        triggers: &["subscription", "profile", "proxy", "node", "group"],
        priority: 75,
        body: include_str!("../../doc/agent/subscriptions.md"),
    },
    AgentDoc {
        id: AgentDocId::LlmProviders,
        title: "LLM providers",
        triggers: &[
            "llm",
            "provider",
            "api key",
            "api_key",
            "model",
            "coding plan",
            "token plan",
            "base_url",
        ],
        priority: 70,
        body: include_str!("../../doc/agent/llm-providers-cn.md"),
    },
    AgentDoc {
        id: AgentDocId::Troubleshooting,
        title: "troubleshooting",
        triggers: &["error", "failed", "offline", "unavailable", "log", "why"],
        priority: 70,
        body: include_str!("../../doc/agent/troubleshooting.md"),
    },
];

pub fn select_docs(message: &str) -> Vec<&'static AgentDoc> {
    let text = message.to_ascii_lowercase();
    let mut docs = Vec::new();
    push_doc(&mut docs, AgentDocId::Overview);
    push_doc(&mut docs, AgentDocId::ConfigSemantics);
    push_doc(&mut docs, AgentDocId::PatchRules);

    for doc in AGENT_DOCS {
        if doc
            .triggers
            .iter()
            .any(|trigger| text.contains(&trigger.to_ascii_lowercase()))
        {
            push_doc(&mut docs, doc.id);
        }
    }

    docs.sort_by_key(|doc| std::cmp::Reverse(doc.priority));
    docs.dedup_by_key(|doc| doc.id);
    docs
}

fn push_doc(docs: &mut Vec<&'static AgentDoc>, id: AgentDocId) {
    if docs.iter().any(|doc| doc.id == id) {
        return;
    }
    if let Some(doc) = AGENT_DOCS.iter().find(|doc| doc.id == id) {
        docs.push(doc);
    }
}

pub fn render_docs(docs: &[&AgentDoc]) -> String {
    docs.iter()
        .map(|doc| format!("## {}\n\n{}", doc.title, doc.body.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_docs_are_present() {
        for doc in AGENT_DOCS {
            assert!(!doc.body.trim().is_empty());
            assert!(doc.body.len() < 12 * 1024);
        }
    }

    #[test]
    fn dns_question_selects_dns_docs() {
        let docs = select_docs("taobao.net DNS nameserver-policy");
        assert!(docs.iter().any(|doc| doc.id == AgentDocId::Dns));
        assert!(docs.iter().any(|doc| doc.id == AgentDocId::PatchRules));
    }

    #[test]
    fn tun_question_selects_tun_docs() {
        let docs = select_docs("why is TUN unavailable");
        assert!(docs.iter().any(|doc| doc.id == AgentDocId::Tun));
        assert!(docs.iter().any(|doc| doc.id == AgentDocId::Troubleshooting));
    }
}
