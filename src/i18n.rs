use clap::ValueEnum;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Language {
    #[value(name = "en", alias = "english")]
    #[default]
    En,
    #[value(name = "zh-CN", alias = "zh-cn", alias = "zh", alias = "cn")]
    ZhCn,
}

impl Language {
    pub const fn is_zh_cn(self) -> bool {
        matches!(self, Self::ZhCn)
    }

    pub const fn assistant_rule(self) -> &'static str {
        match self {
            Self::En => {
                "Reply in English unless the user explicitly asks for another language. Keep technical identifiers unchanged."
            }
            Self::ZhCn => {
                "默认使用简体中文回答，除非用户明确要求其他语言。保留 LLM、Runtime、DNS、TUN、Provider、Model、Base URL、Port Proxy、mihomo、config 等专业术语和标识符。"
            }
        }
    }
}
