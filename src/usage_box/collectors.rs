//! The Omarchy usage collectors (`omarchy-agent-usage-<id>`), their paddock
//! profiles, the home folders and login files they read, and the hosts they
//! call.

/// One `omarchy-agent-usage-<id>` collector, its paddock profile (when one
/// exists), the home folders and login files it reads, and the hosts it calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collector {
    pub id: &'static str,
    pub profile: Option<&'static str>,
    pub folders: &'static [&'static str],
    pub login_files: &'static [&'static str],
    pub hosts: &'static [&'static str],
}

const fn col(
    id: &'static str,
    profile: Option<&'static str>,
    folders: &'static [&'static str],
    login_files: &'static [&'static str],
    hosts: &'static [&'static str],
) -> Collector {
    Collector {
        id,
        profile,
        folders,
        login_files,
        hosts,
    }
}

pub const COLLECTORS: &[Collector] = &[
    col(
        "agy",
        Some("agy"),
        &[".gemini"],
        &[
            ".gemini/antigravity-cli/antigravity-oauth-token",
            ".gemini/antigravity-cli/mcp_oauth_tokens.json",
        ],
        &[
            "daily-cloudcode-pa.googleapis.com",
            "generativelanguage.googleapis.com",
            "oauth2.googleapis.com",
            "cloudcode-pa.googleapis.com",
        ],
    ),
    col(
        "claude",
        Some("claude"),
        &[".claude", ".claude.json"],
        &[
            ".claude/.credentials.json",
            ".claude.json",
            ".claude/.config.json",
        ],
        &[
            "api.anthropic.com",
            "claude.ai",
            "console.anthropic.com",
            "statsig.anthropic.com",
        ],
    ),
    col(
        "codex",
        Some("codex"),
        &[".codex"],
        &[".codex/auth.json", ".codex/.credentials.json"],
        &["api.openai.com", "chatgpt.com", "auth.openai.com"],
    ),
    col(
        "copilot",
        Some("copilot"),
        &[".copilot", ".config/github-copilot", ".config/gh"],
        &[
            ".copilot/config.json",
            ".config/github-copilot/hosts.json",
            ".config/github-copilot/apps.json",
            ".config/gh/hosts.yml",
        ],
        &["api.github.com", "api.githubcopilot.com", "github.com"],
    ),
    col(
        "cursor",
        Some("cursor-agent"),
        &[".cursor", ".config/cursor", ".config/Cursor"],
        &[".config/cursor/auth.json"],
        &["api2.cursor.sh"],
    ),
    col(
        "deepseek",
        Some("dsh"),
        &[".dsh"],
        &[".dsh/.credentials.yaml", ".dsh/.env"],
        &["api.deepseek.com"],
    ),
    col(
        "devin",
        Some("devin"),
        &[".local/share/devin", ".config/devin"],
        &[".local/share/devin/credentials.toml"],
        &[],
    ),
    col(
        "fireworks",
        None,
        &[".fireworks"],
        &[".fireworks/auth.ini"],
        &["api.fireworks.ai"],
    ),
    col(
        "grok",
        Some("grok"),
        &[".grok"],
        &[".grok/auth.json", ".grok/mcp_credentials.json"],
        &["cli-chat-proxy.grok.com", "api.x.ai", "x.ai", "grok.com"],
    ),
    col(
        "grokbot",
        Some("cursor-agent"),
        &[".config/cursor", ".config/Cursor"],
        &[".config/cursor/auth.json"],
        &["api2.cursor.sh"],
    ),
    col(
        "hermes",
        Some("hermes"),
        &[".hermes"],
        &[".hermes/.env", ".hermes/auth.json"],
        &[],
    ),
    col(
        "kimchi",
        None,
        &[".config/kimchi"],
        &[".config/kimchi/config.json"],
        &["llm.kimchi.dev", "llm.eu.kimchi.dev"],
    ),
    col(
        "kimi",
        Some("kimi"),
        &[".kimi-code", ".kimi"],
        &[
            ".kimi-code/credentials/kimi-code.json",
            ".kimi/credentials/kimi-code.json",
        ],
        &["api.kimi.com", "auth.kimi.com"],
    ),
    col("local", None, &[], &[], &[]),
    col(
        "minimax",
        Some("mcode"),
        &[".minimax", ".mmx"],
        &[
            ".mmx/config.json",
            ".mmx/config.yaml",
            ".minimax/config.yaml",
        ],
        &["api.minimax.io", "api.minimaxi.com"],
    ),
    col(
        "muse",
        Some("muse"),
        &[".config/muse", ".local/share/muse"],
        &[".config/muse/auth.json"],
        &["api.meta.ai"],
    ),
    col("ollama", None, &[], &[], &["ollama.com"]),
    col(
        "opencode",
        Some("opencode"),
        &[".local/share/opencode", ".config/opencode"],
        &[
            ".local/share/opencode/auth.json",
            ".local/share/opencode/mcp-auth.json",
        ],
        &["opencode.ai"],
    ),
    col("openrouter", None, &[], &[], &["openrouter.ai"]),
    col(
        "qwen",
        Some("qwen"),
        &[".qwen"],
        &[".qwen/oauth_creds.json", ".qwen/.env"],
        &[],
    ),
    col("synthetic", None, &[], &[], &["api.synthetic.new"]),
    col(
        "vibe",
        None,
        &[".vibe"],
        &[".vibe/.env", ".vibe/config.toml"],
        &[],
    ),
    col("zai", None, &[], &[], &["api.z.ai", "open.bigmodel.cn"]),
];

pub fn collector(id: &str) -> Option<&'static Collector> {
    COLLECTORS.iter().find(|c| c.id == id)
}
