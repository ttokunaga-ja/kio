//! Rust-owned, compact and environment-free persona corpus plan.
use kio_core::cas::{canonical_json_bytes, hash_bytes};
use serde::{Deserialize, Deserializer, Serialize, de::Visitor};
use std::collections::{BTreeSet, VecDeque};
use thiserror::Error;

pub const SCHEMA: &str = "kio.persona.plan/v2";
pub const FIXTURE_ID: &str = "kio-persona-pc-v2";
pub const SEED: u64 = 20_260_713;
pub const PERSONA_COUNT: usize = 20;
pub const SCOPES_PER_PERSON: usize = 20;
/// Frozen plans are below this bound; it limits untrusted CLI plan input.
pub const MAX_CANONICAL_BYTES: usize = 4 * 1024 * 1024;
pub const TINY_PLAN_HASH: &str =
    "sha256:984633624c1a65e7873827937f9af0351bc14c9bca9b66f5032b3e3fe790e809";
pub const PILOT_PLAN_HASH: &str =
    "sha256:82bf41b163e36f7698ba78266f366d640fc8fd7967445a2b684e39aad2bfb480";
pub const FULL_PLAN_HASH: &str =
    "sha256:eeb6ccb6c7964b8885217aa87ad22728a238ff216d3553b85cffe78411e19cdb";
const WEIGHTS: [u32; 20] = [10, 9, 8, 8, 7, 6, 6, 5, 5, 4, 4, 3, 5, 4, 4, 3, 3, 2, 2, 2];
const SECONDARY: [&str; 8] = [
    "desktop/working",
    "documents/reference",
    "downloads/inbox",
    "downloads/exports",
    "cloud/my-files",
    "cloud/team-shared",
    "mail/recent",
    "archive/closed",
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PersonaPlanError {
    #[error("serialization: {0}")]
    Serialize(String),
    #[error("JSON: {0}")]
    Json(String),
    #[error("noncanonical JCS+LF")]
    NonCanonical,
    #[error("invalid persona plan: {0}")]
    Invalid(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum PersonaProfile {
    Tiny,
    Pilot,
    Full,
}
impl PersonaProfile {
    const fn raw(self, full: u32) -> u32 {
        match self {
            Self::Tiny => 200,
            Self::Pilot => 1000,
            Self::Full => full,
        }
    }
    const fn chunks(self) -> Option<u32> {
        match self {
            Self::Tiny => None,
            Self::Pilot => Some(12_000),
            Self::Full => Some(120_000),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PersonaId {
    P01,
    P02,
    P03,
    P04,
    P05,
    P06,
    P07,
    P08,
    P09,
    P10,
    P11,
    P12,
    P13,
    P14,
    P15,
    P16,
    P17,
    P18,
    P19,
    P20,
}
impl PersonaId {
    pub const ALL: [Self; 20] = [
        Self::P01,
        Self::P02,
        Self::P03,
        Self::P04,
        Self::P05,
        Self::P06,
        Self::P07,
        Self::P08,
        Self::P09,
        Self::P10,
        Self::P11,
        Self::P12,
        Self::P13,
        Self::P14,
        Self::P15,
        Self::P16,
        Self::P17,
        Self::P18,
        Self::P19,
        Self::P20,
    ];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::P01 => "p01",
            Self::P02 => "p02",
            Self::P03 => "p03",
            Self::P04 => "p04",
            Self::P05 => "p05",
            Self::P06 => "p06",
            Self::P07 => "p07",
            Self::P08 => "p08",
            Self::P09 => "p09",
            Self::P10 => "p10",
            Self::P11 => "p11",
            Self::P12 => "p12",
            Self::P13 => "p13",
            Self::P14 => "p14",
            Self::P15 => "p15",
            Self::P16 => "p16",
            Self::P17 => "p17",
            Self::P18 => "p18",
            Self::P19 => "p19",
            Self::P20 => "p20",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatFamily {
    Md,
    TxtLog,
    StructuredText,
    CsvTsv,
    HtmlEml,
    Ipynb,
    PdfText,
    PdfScan,
    Docx,
    Xlsx,
    Pptx,
    Image,
    Media,
    DomainBinary,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FormatVariant {
    Md,
    Markdown,
    Txt,
    Log,
    Jsonl,
    Json,
    Yaml,
    Xml,
    Sql,
    Csv,
    Tsv,
    Html,
    Eml,
    Ipynb,
    PdfText,
    PdfScan,
    Docx,
    Xlsx,
    Pptx,
    Png,
    Wav,
    Pcap,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WavePurpose {
    Baseline,
    DailyWork,
    Reorganization,
    Milestone,
    Closure,
    Retention,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Create,
    Index,
    Edit,
    Rename,
    Move,
    Duplicate,
    Derive,
    Archive,
    Delete,
    Restore,
    Purge,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    IndexAuto,
    PurgedCommit,
    IndexNoop,
    None,
}
impl FormatFamily {
    pub const ALL: [Self; 14] = [
        Self::Md,
        Self::TxtLog,
        Self::StructuredText,
        Self::CsvTsv,
        Self::HtmlEml,
        Self::Ipynb,
        Self::PdfText,
        Self::PdfScan,
        Self::Docx,
        Self::Xlsx,
        Self::Pptx,
        Self::Image,
        Self::Media,
        Self::DomainBinary,
    ];
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateRole {
    ContractContributor,
    IncidentalSearchable,
    RawOnly,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    LocalText,
    IncidentalSniff,
    LocalPdfText,
    AwaitingOcr,
    AwaitConversion,
    UnsupportedBinary,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Primary,
    Secondary,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Cohort {
    P,
    X,
    Y,
    N,
    U,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Wave {
    W0,
    W1,
    W2,
    W3,
    W4,
    W5,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuralKind {
    SameScopeRename,
    CrossScopeMove,
    Create,
    ExactDuplicate,
    NearDuplicate,
    DerivedFormat,
    ArchiveMove,
    DeleteForRestore,
    RestoreToActiveScope,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransformKind {
    CanonicalSource,
    NearPngOneChannel,
    PngToScanPdf,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaPlan {
    pub schema: String,
    pub fixture_id: String,
    pub seed: u64,
    pub profile: PersonaProfile,
    #[serde(deserialize_with = "bounded_20")]
    pub personas: Vec<PersonPlan>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonPlan {
    pub id: PersonaId,
    pub role: String,
    pub raw_files: u32,
    pub current_chunks: u32,
    #[serde(deserialize_with = "bounded_14")]
    pub formats: Vec<FamilyPlan>,
    #[serde(deserialize_with = "bounded_22")]
    pub variants: Vec<VariantPlan>,
    #[serde(deserialize_with = "bounded_20")]
    pub scopes: Vec<ScopePlan>,
    #[serde(deserialize_with = "bounded_5")]
    pub cohorts: Vec<CohortPlan>,
    #[serde(deserialize_with = "bounded_30")]
    pub structural: Vec<StructuralPlan>,
    #[serde(deserialize_with = "bounded_6")]
    pub waves: Vec<WavePlan>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FamilyPlan {
    pub family: FormatFamily,
    pub percentage: u8,
    pub files: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantPlan {
    pub variant: FormatVariant,
    pub family: FormatFamily,
    pub percentage: u8,
    pub gate_role: GateRole,
    pub disposition: Disposition,
    pub files: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopePlan {
    pub id: String,
    pub kind: ScopeKind,
    pub path: String,
    pub raw_files: u32,
    pub current_chunks: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CohortPlan {
    pub cohort: Cohort,
    pub source_count: u32,
    pub chunks: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuralPlan {
    pub event_id: String,
    pub wave: Wave,
    pub ordinal: u8,
    pub kind: StructuralKind,
    pub source_id: String,
    pub parent_source_id: Option<String>,
    pub source_scope_id: Option<String>,
    pub destination_scope_id: Option<String>,
    pub child_variant: Option<FormatVariant>,
    #[serde(deserialize_with = "bounded_16")]
    pub depends_on: Vec<String>,
    pub paired_event_id: Option<String>,
    pub history_neutral: bool,
    pub requires_raw_only: bool,
    pub transform: TransformKind,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WavePlan {
    pub wave: Wave,
    pub purpose: WavePurpose,
    #[serde(deserialize_with = "bounded_16")]
    pub operations: Vec<Operation>,
    #[serde(deserialize_with = "bounded_16")]
    pub boundaries: Vec<Boundary>,
    pub history_chunks: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceProjection {
    pub source_id: String,
    pub ordinal: u32,
    pub scope_id: String,
    pub variant: FormatVariant,
    pub gate_role: GateRole,
    pub disposition: Disposition,
    pub planned_chunks: u32,
    pub cohort: Option<Cohort>,
}

struct Row {
    id: PersonaId,
    role: &'static str,
    full: u32,
    ratios: [u8; 14],
}

fn primary_paths(id: PersonaId) -> &'static [&'static str; 12] {
    match id {
        PersonaId::P01 => &[
            "documents/work/product-alpha/architecture",
            "documents/work/product-alpha/api-contracts",
            "documents/work/product-alpha/release-notes",
            "documents/work/product-beta/architecture",
            "documents/work/product-beta/api-contracts",
            "repos/product-alpha/docs",
            "repos/product-beta/docs",
            "work-items/decision-records",
            "work-items/code-reviews",
            "meetings/engineering",
            "vendor-docs/platforms",
            "operations/migration-notes",
        ],
        PersonaId::P02 => &[
            "documents/operations/runbooks",
            "documents/operations/postmortems",
            "infrastructure/kubernetes",
            "infrastructure/terraform",
            "services/checkout/prod/oncall/operations",
            "services/identity/prod/oncall/operations",
            "observability/alerts",
            "observability/dashboards",
            "observability/log-exports",
            "changes/deployments",
            "capacity/reports",
            "meetings/operations",
        ],
        PersonaId::P03 => &[
            "security/threat-models",
            "security/pentest-reports",
            "security/vulnerabilities",
            "security/incident-reports",
            "compliance/policies",
            "compliance/control-evidence",
            "compliance/audit-requests",
            "vendor-risk/questionnaires",
            "soc/siem-exports",
            "soc/detection-rules",
            "privacy/risk-assessments",
            "meetings/security-reviews",
        ],
        PersonaId::P04 => &[
            "research/papers",
            "research/literature-notes",
            "research/programs/model-alpha/experiments/configs",
            "research/programs/model-alpha/experiments/results",
            "research/programs/model-beta/experiments/configs",
            "research/programs/model-beta/experiments/results",
            "notebooks/exports",
            "datasets/cards",
            "models/model-cards",
            "evaluations/benchmarks",
            "presentations/lab-meetings",
            "repos/ml-project/docs",
        ],
        PersonaId::P05 => &[
            "analytics/sql/production",
            "analytics/sql/ad-hoc",
            "analytics/data-dictionary",
            "analytics/lineage",
            "dashboards/sales",
            "dashboards/product",
            "reports/weekly",
            "reports/monthly",
            "forecasts/scenarios",
            "requests/stakeholder",
            "exports/warehouse",
            "meetings/metric-reviews",
        ],
        PersonaId::P06 => &[
            "lab/lab-notebooks",
            "lab/protocols",
            "programs/study-alpha/2026/cohort-a/raw-exports",
            "programs/study-alpha/2026/cohort-a/analysis",
            "programs/study-beta/2026/cohort-b/raw-exports",
            "programs/study-beta/2026/cohort-b/analysis",
            "instruments/calibration",
            "samples/manifests",
            "literature/papers",
            "grants/applications",
            "figures/manuscript",
            "meetings/lab",
        ],
        PersonaId::P07 => &[
            "research/primary-sources",
            "research/archive-scans",
            "research/ocr-transcripts",
            "research/bibliography-exports",
            "notes/source-annotations",
            "notes/literature",
            "dissertation/chapter-01",
            "dissertation/chapter-02",
            "dissertation/appendices",
            "translations/working",
            "conferences/presentations",
            "correspondence/archive-requests",
        ],
        PersonaId::P08 => &[
            "portfolio/product-alpha/2026/q3/prds",
            "portfolio/product-alpha/2026/q3/discovery",
            "portfolio/product-beta/2026/q4/prds",
            "portfolio/product-beta/2026/q4/discovery",
            "roadmap/quarterly",
            "roadmap/dependencies",
            "customer-feedback/interviews",
            "customer-feedback/support-summaries",
            "analytics/product-metrics",
            "launches/release-plans",
            "decisions/meeting-notes",
            "research/market",
        ],
        PersonaId::P09 => &[
            "research/study-alpha/plans",
            "research/study-alpha/transcripts",
            "research/study-alpha/findings",
            "research/study-beta/plans",
            "research/study-beta/transcripts",
            "research/study-beta/findings",
            "surveys/results",
            "design/prototype-specs",
            "design/figma-exports",
            "personas/journey-maps",
            "recordings/transcript-sidecars",
            "consent/synthetic-records",
        ],
        PersonaId::P10 => &[
            "engagements/client-alpha/2026/phase-1/data-room",
            "engagements/client-alpha/2026/phase-1/interviews",
            "engagements/client-alpha/2026/phase-1/analysis",
            "engagements/client-alpha/2026/phase-1/deliverables",
            "engagements/client-beta/2026/phase-2/data-room",
            "engagements/client-beta/2026/phase-2/interviews",
            "engagements/client-beta/2026/phase-2/analysis",
            "engagements/client-beta/2026/phase-2/deliverables",
            "proposals/active",
            "benchmarks/industry",
            "templates/consulting",
            "meetings/internal-reviews",
        ],
        PersonaId::P11 => &[
            "accounts/account-alpha/plans",
            "accounts/account-alpha/calls",
            "accounts/account-alpha/proposals",
            "accounts/account-beta/plans",
            "accounts/account-beta/calls",
            "accounts/account-beta/proposals",
            "opportunities/pipeline",
            "rfp/responses",
            "pricing/approved",
            "contracts/drafts",
            "contracts/executed",
            "travel/meeting-notes",
        ],
        PersonaId::P12 => &[
            "support/ticket-exports",
            "support/escalations",
            "support/known-issues",
            "knowledge-base/drafts",
            "knowledge-base/published",
            "customers/customer-alpha/qbr",
            "customers/customer-alpha/case-history",
            "customers/customer-beta/qbr",
            "customers/customer-beta/case-history",
            "logs/customer-attachments",
            "macros/replies",
            "incidents/support-links",
        ],
        PersonaId::P13 => &[
            "matters/matter-alpha/correspondence",
            "matters/matter-alpha/working",
            "matters/matter-beta/correspondence",
            "matters/matter-beta/working",
            "contracts/drafts",
            "contracts/executed",
            "contracts/templates",
            "regulations/guidance",
            "policies/privacy",
            "due-diligence/data-room",
            "legal-hold/notices",
            "board/legal-reports",
        ],
        PersonaId::P14 => &[
            "finance/close/2026/q1/2026-01",
            "finance/close/2026/q1/2026-02",
            "finance/close/2026/q1/2026-03",
            "budget/annual",
            "forecasts/base-case",
            "forecasts/scenarios",
            "invoices/vendor",
            "expenses/department",
            "audit/evidence",
            "board/finance-packs",
            "models/operating-model",
            "erp/exports",
        ],
        PersonaId::P15 => &[
            "recruiting/requisition-alpha/candidates",
            "recruiting/requisition-alpha/interviews",
            "recruiting/requisition-beta/candidates",
            "recruiting/requisition-beta/interviews",
            "recruiting/offers",
            "people/policies",
            "people/headcount",
            "people/performance-synthetic",
            "people/surveys-synthetic",
            "learning/training",
            "compensation/bands",
            "compliance/retention",
        ],
        PersonaId::P16 => &[
            "clinical/studies/study-alpha/2026/protocols",
            "clinical/studies/study-alpha/2026/synthetic-cases",
            "clinical/studies/study-alpha/2026/results",
            "clinical/studies/study-beta/2026/protocols",
            "clinical/studies/study-beta/2026/synthetic-cases",
            "clinical/studies/study-beta/2026/results",
            "guidelines/clinical",
            "literature/papers",
            "regulatory/submissions",
            "safety/adverse-events-synthetic",
            "statistics/analysis",
            "presentations/grand-rounds",
        ],
        PersonaId::P17 => &[
            "portfolio/projects/project-alpha/2026/construction/drawings",
            "portfolio/projects/project-alpha/2026/construction/specifications",
            "portfolio/projects/project-alpha/2026/construction/rfi",
            "portfolio/projects/project-alpha/2026/construction/submittals",
            "portfolio/projects/project-alpha/2026/construction/change-orders",
            "portfolio/projects/project-alpha/2026/construction/site-reports",
            "portfolio/projects/project-beta/2026/construction/drawings",
            "portfolio/projects/project-beta/2026/construction/specifications",
            "portfolio/projects/project-beta/2026/construction/rfi",
            "portfolio/projects/project-beta/2026/construction/submittals",
            "bim/exports",
            "meetings/site",
        ],
        PersonaId::P18 => &[
            "products/product-alpha/fmea",
            "products/product-alpha/test-results",
            "products/product-alpha/capa",
            "products/product-beta/fmea",
            "products/product-beta/test-results",
            "products/product-beta/capa",
            "quality/sop",
            "quality/work-instructions",
            "quality/nonconformance",
            "suppliers/audits",
            "suppliers/certificates",
            "engineering/change-orders",
        ],
        PersonaId::P19 => &[
            "learning/courses/course-alpha/2026/term-1/lesson-plans",
            "learning/courses/course-alpha/2026/term-1/readings",
            "learning/courses/course-alpha/2026/term-1/assignments",
            "learning/courses/course-alpha/2026/term-1/synthetic-student-work",
            "learning/courses/course-beta/2026/term-2/lesson-plans",
            "learning/courses/course-beta/2026/term-2/readings",
            "learning/courses/course-beta/2026/term-2/assignments",
            "learning/courses/course-beta/2026/term-2/synthetic-student-work",
            "assessments/item-bank",
            "lms/exports",
            "presentations/lectures",
            "professional-development/notes",
        ],
        PersonaId::P20 => &[
            "newsroom/investigations/story-alpha/2026/sources",
            "newsroom/investigations/story-alpha/2026/transcripts",
            "newsroom/investigations/story-alpha/2026/foia",
            "newsroom/investigations/story-alpha/2026/drafts",
            "newsroom/investigations/story-alpha/2026/fact-check",
            "newsroom/investigations/story-beta/2026/sources",
            "newsroom/investigations/story-beta/2026/transcripts",
            "newsroom/investigations/story-beta/2026/foia",
            "newsroom/investigations/story-beta/2026/drafts",
            "data/analysis",
            "media/transcript-sidecars",
            "pitches/research",
        ],
    }
}
const ROWS: [Row; 20] = [
    Row {
        id: PersonaId::P01,
        role: "software-engineer",
        full: 12000,
        ratios: [50, 8, 12, 3, 5, 1, 7, 1, 3, 2, 2, 3, 0, 3],
    },
    Row {
        id: PersonaId::P02,
        role: "site-reliability-engineer",
        full: 15000,
        ratios: [35, 22, 20, 5, 3, 0, 4, 0, 2, 1, 1, 2, 0, 5],
    },
    Row {
        id: PersonaId::P03,
        role: "security-grc-analyst",
        full: 10000,
        ratios: [18, 12, 15, 10, 8, 0, 15, 5, 5, 4, 2, 3, 0, 3],
    },
    Row {
        id: PersonaId::P04,
        role: "ml-research-engineer",
        full: 10000,
        ratios: [30, 7, 10, 12, 2, 12, 12, 1, 2, 3, 3, 5, 0, 1],
    },
    Row {
        id: PersonaId::P05,
        role: "bi-data-analyst",
        full: 12000,
        ratios: [14, 5, 14, 20, 5, 5, 5, 1, 3, 15, 4, 3, 0, 6],
    },
    Row {
        id: PersonaId::P06,
        role: "life-science-researcher",
        full: 8000,
        ratios: [9, 6, 5, 15, 2, 3, 18, 8, 8, 8, 5, 9, 0, 4],
    },
    Row {
        id: PersonaId::P07,
        role: "humanities-researcher",
        full: 7000,
        ratios: [12, 10, 4, 3, 5, 0, 25, 20, 10, 1, 2, 6, 1, 1],
    },
    Row {
        id: PersonaId::P08,
        role: "product-manager",
        full: 8000,
        ratios: [11, 4, 5, 8, 8, 0, 13, 3, 15, 8, 15, 7, 1, 2],
    },
    Row {
        id: PersonaId::P09,
        role: "ux-researcher",
        full: 9000,
        ratios: [8, 15, 4, 8, 3, 0, 10, 4, 12, 4, 8, 15, 7, 2],
    },
    Row {
        id: PersonaId::P10,
        role: "management-consultant",
        full: 7000,
        ratios: [4, 4, 2, 8, 6, 0, 18, 5, 12, 18, 18, 3, 0, 2],
    },
    Row {
        id: PersonaId::P11,
        role: "account-executive",
        full: 10000,
        ratios: [3, 4, 2, 5, 25, 0, 16, 4, 14, 7, 10, 5, 3, 2],
    },
    Row {
        id: PersonaId::P12,
        role: "support-success-lead",
        full: 16000,
        ratios: [19, 20, 15, 12, 12, 0, 5, 1, 3, 2, 1, 7, 1, 2],
    },
    Row {
        id: PersonaId::P13,
        role: "corporate-privacy-counsel",
        full: 7000,
        ratios: [3, 4, 1, 2, 14, 0, 28, 15, 22, 3, 2, 3, 0, 3],
    },
    Row {
        id: PersonaId::P14,
        role: "finance-controller",
        full: 9000,
        ratios: [4, 3, 4, 15, 5, 0, 13, 8, 8, 27, 7, 3, 0, 3],
    },
    Row {
        id: PersonaId::P15,
        role: "recruiter-people-ops",
        full: 8000,
        ratios: [4, 5, 2, 7, 15, 0, 20, 8, 20, 8, 3, 5, 1, 2],
    },
    Row {
        id: PersonaId::P16,
        role: "clinical-researcher",
        full: 8000,
        ratios: [6, 6, 4, 10, 4, 1, 24, 12, 10, 8, 5, 6, 1, 3],
    },
    Row {
        id: PersonaId::P17,
        role: "construction-project-manager",
        full: 8000,
        ratios: [3, 4, 2, 5, 4, 0, 20, 12, 8, 10, 4, 12, 1, 15],
    },
    Row {
        id: PersonaId::P18,
        role: "manufacturing-quality-engineer",
        full: 12000,
        ratios: [8, 12, 6, 15, 3, 0, 18, 6, 8, 10, 3, 5, 0, 6],
    },
    Row {
        id: PersonaId::P19,
        role: "educator-instructional-designer",
        full: 9000,
        ratios: [8, 5, 2, 5, 5, 0, 20, 8, 15, 7, 12, 8, 3, 2],
    },
    Row {
        id: PersonaId::P20,
        role: "investigative-journalist",
        full: 10000,
        ratios: [9, 18, 3, 8, 10, 0, 16, 10, 8, 2, 2, 8, 4, 2],
    },
];
const VS: [(FormatVariant, FormatFamily, u8, GateRole, Disposition); 22] = [
    (
        FormatVariant::Md,
        FormatFamily::Md,
        70,
        GateRole::ContractContributor,
        Disposition::LocalText,
    ),
    (
        FormatVariant::Markdown,
        FormatFamily::Md,
        30,
        GateRole::ContractContributor,
        Disposition::LocalText,
    ),
    (
        FormatVariant::Txt,
        FormatFamily::TxtLog,
        70,
        GateRole::ContractContributor,
        Disposition::LocalText,
    ),
    (
        FormatVariant::Log,
        FormatFamily::TxtLog,
        20,
        GateRole::IncidentalSearchable,
        Disposition::IncidentalSniff,
    ),
    (
        FormatVariant::Jsonl,
        FormatFamily::TxtLog,
        10,
        GateRole::IncidentalSearchable,
        Disposition::IncidentalSniff,
    ),
    (
        FormatVariant::Json,
        FormatFamily::StructuredText,
        35,
        GateRole::IncidentalSearchable,
        Disposition::IncidentalSniff,
    ),
    (
        FormatVariant::Yaml,
        FormatFamily::StructuredText,
        25,
        GateRole::IncidentalSearchable,
        Disposition::IncidentalSniff,
    ),
    (
        FormatVariant::Xml,
        FormatFamily::StructuredText,
        20,
        GateRole::IncidentalSearchable,
        Disposition::IncidentalSniff,
    ),
    (
        FormatVariant::Sql,
        FormatFamily::StructuredText,
        20,
        GateRole::IncidentalSearchable,
        Disposition::IncidentalSniff,
    ),
    (
        FormatVariant::Csv,
        FormatFamily::CsvTsv,
        70,
        GateRole::IncidentalSearchable,
        Disposition::IncidentalSniff,
    ),
    (
        FormatVariant::Tsv,
        FormatFamily::CsvTsv,
        30,
        GateRole::IncidentalSearchable,
        Disposition::IncidentalSniff,
    ),
    (
        FormatVariant::Html,
        FormatFamily::HtmlEml,
        60,
        GateRole::IncidentalSearchable,
        Disposition::IncidentalSniff,
    ),
    (
        FormatVariant::Eml,
        FormatFamily::HtmlEml,
        40,
        GateRole::IncidentalSearchable,
        Disposition::IncidentalSniff,
    ),
    (
        FormatVariant::Ipynb,
        FormatFamily::Ipynb,
        100,
        GateRole::IncidentalSearchable,
        Disposition::IncidentalSniff,
    ),
    (
        FormatVariant::PdfText,
        FormatFamily::PdfText,
        100,
        GateRole::ContractContributor,
        Disposition::LocalPdfText,
    ),
    (
        FormatVariant::PdfScan,
        FormatFamily::PdfScan,
        100,
        GateRole::RawOnly,
        Disposition::AwaitingOcr,
    ),
    (
        FormatVariant::Docx,
        FormatFamily::Docx,
        100,
        GateRole::RawOnly,
        Disposition::AwaitConversion,
    ),
    (
        FormatVariant::Xlsx,
        FormatFamily::Xlsx,
        100,
        GateRole::RawOnly,
        Disposition::AwaitConversion,
    ),
    (
        FormatVariant::Pptx,
        FormatFamily::Pptx,
        100,
        GateRole::RawOnly,
        Disposition::AwaitConversion,
    ),
    (
        FormatVariant::Png,
        FormatFamily::Image,
        100,
        GateRole::RawOnly,
        Disposition::AwaitingOcr,
    ),
    (
        FormatVariant::Wav,
        FormatFamily::Media,
        100,
        GateRole::RawOnly,
        Disposition::UnsupportedBinary,
    ),
    (
        FormatVariant::Pcap,
        FormatFamily::DomainBinary,
        100,
        GateRole::RawOnly,
        Disposition::UnsupportedBinary,
    ),
];
fn h(total: u32, w: &[u32]) -> Vec<u32> {
    let d: u32 = w.iter().sum();
    let mut r: Vec<u32> = w.iter().map(|x| total * x / d).collect();
    let mut o: Vec<_> = (0..w.len()).collect();
    o.sort_by_key(|&i| (std::cmp::Reverse(total * w[i] % d), i));
    for i in o.into_iter().take((total - r.iter().sum::<u32>()) as usize) {
        r[i] += 1
    }
    r
}

fn plan(r: &Row, profile: PersonaProfile) -> PersonPlan {
    let raw = profile.raw(r.full);
    let ff = h(
        raw,
        &r.ratios.iter().map(|v| u32::from(*v)).collect::<Vec<_>>(),
    );
    let mut vf = [0; 22];
    for (i, f) in FormatFamily::ALL.into_iter().enumerate() {
        let ix: Vec<_> = VS.iter().enumerate().filter(|(_, v)| v.1 == f).collect();
        for ((j, _), n) in ix.into_iter().zip(h(
            ff[i],
            &VS.iter()
                .filter(|v| v.1 == f)
                .map(|v| u32::from(v.2))
                .collect::<Vec<_>>(),
        )) {
            vf[j] = n
        }
    }
    let contrib: u32 = VS
        .iter()
        .enumerate()
        .filter(|(_, v)| v.3 == GateRole::ContractContributor)
        .map(|(i, _)| vf[i])
        .sum();
    let chunks = profile.chunks().unwrap_or(contrib * 3);
    let sf = h(raw, &WEIGHTS);
    let sc = h(chunks, &WEIGHTS);
    let scopes = (0..20)
        .map(|i| {
            let pri = i < 12;
            let num = if pri { i + 1 } else { i - 11 };
            ScopePlan {
                id: format!(
                    "{}-{}-{num:02}",
                    r.id.as_str(),
                    if pri { "primary" } else { "secondary" }
                ),
                kind: if pri {
                    ScopeKind::Primary
                } else {
                    ScopeKind::Secondary
                },
                path: if pri {
                    primary_paths(r.id)[i].to_owned()
                } else {
                    SECONDARY[i - 12].into()
                },
                raw_files: sf[i],
                current_chunks: sc[i],
            }
        })
        .collect();
    let p = chunks * 4 / 100;
    let x = chunks * 10 / 100;
    let y = chunks * 20 / 100 - p - x;
    let n = p;
    let u = chunks - p - x - y - n;
    let ns = [p, x, y, n, u];
    let cohort_sources = h(contrib, &[4, 10, 6, 4, 76]);
    let cohorts: Vec<CohortPlan> = [Cohort::P, Cohort::X, Cohort::Y, Cohort::N, Cohort::U]
        .into_iter()
        .zip(ns.into_iter().zip(cohort_sources))
        .map(|(cohort, (chunks, source_count))| CohortPlan {
            cohort,
            chunks,
            source_count,
        })
        .collect();
    let hist = [
        0,
        p + x + y,
        p + x + y,
        p + x + y + x + y + n,
        p + x + y + x + y + n + x,
        p + x + y + x + y + n + x,
    ];
    let specs = [
        (
            WavePurpose::Baseline,
            &[Operation::Create, Operation::Index][..],
            &[Boundary::IndexAuto][..],
        ),
        (
            WavePurpose::DailyWork,
            &[
                Operation::Create,
                Operation::Edit,
                Operation::Rename,
                Operation::Move,
            ][..],
            &[Boundary::IndexAuto][..],
        ),
        (
            WavePurpose::Reorganization,
            &[Operation::Rename, Operation::Move][..],
            &[Boundary::IndexAuto][..],
        ),
        (
            WavePurpose::Milestone,
            &[Operation::Edit, Operation::Duplicate, Operation::Derive][..],
            &[Boundary::IndexAuto][..],
        ),
        (
            WavePurpose::Closure,
            &[Operation::Archive, Operation::Delete, Operation::Create][..],
            &[Boundary::IndexAuto][..],
        ),
        (
            WavePurpose::Retention,
            &[
                Operation::Edit,
                Operation::Restore,
                Operation::Purge,
                Operation::Create,
            ][..],
            &[Boundary::PurgedCommit, Boundary::IndexNoop, Boundary::None][..],
        ),
    ];
    let mut person = PersonPlan {
        id: r.id,
        role: r.role.into(),
        raw_files: raw,
        current_chunks: chunks,
        formats: FormatFamily::ALL
            .into_iter()
            .enumerate()
            .map(|(i, f)| FamilyPlan {
                family: f,
                percentage: r.ratios[i],
                files: ff[i],
            })
            .collect(),
        variants: VS
            .into_iter()
            .enumerate()
            .map(
                |(i, (variant, family, percentage, gate_role, disposition))| VariantPlan {
                    variant,
                    family,
                    percentage,
                    gate_role,
                    disposition,
                    files: vf[i],
                },
            )
            .collect(),
        scopes,
        cohorts,
        structural: Vec::new(),
        waves: [Wave::W0, Wave::W1, Wave::W2, Wave::W3, Wave::W4, Wave::W5]
            .into_iter()
            .zip(hist)
            .zip(specs)
            .map(
                |((wave, history_chunks), (purpose, ops, boundaries))| WavePlan {
                    wave,
                    purpose,
                    operations: ops.to_vec(),
                    boundaries: boundaries.to_vec(),
                    history_chunks,
                },
            )
            .collect(),
    };
    person.structural = build_structural_events(&person, profile)
        .expect("the frozen persona source inventory satisfies structural allocation");
    person
}
pub fn frozen_plan(profile: PersonaProfile) -> PersonaPlan {
    PersonaPlan {
        schema: SCHEMA.into(),
        fixture_id: FIXTURE_ID.into(),
        seed: SEED,
        profile,
        personas: ROWS.iter().map(|r| plan(r, profile)).collect(),
    }
}

struct StructuralInput {
    wave: Wave,
    ordinal: u8,
    kind: StructuralKind,
    source_id: String,
    parent_source_id: Option<String>,
    source_scope_id: Option<String>,
    destination_scope_id: Option<String>,
    child_variant: Option<FormatVariant>,
    depends_on: Vec<String>,
    paired_event_id: Option<String>,
    requires_raw_only: bool,
    transform: TransformKind,
}

fn wave_name(wave: Wave) -> &'static str {
    match wave {
        Wave::W0 => "W0",
        Wave::W1 => "W1",
        Wave::W2 => "W2",
        Wave::W3 => "W3",
        Wave::W4 => "W4",
        Wave::W5 => "W5",
    }
}

fn structural_event_id(persona: PersonaId, wave: Wave, ordinal: u8) -> String {
    format!("{}-{}-{ordinal:03}", persona.as_str(), wave_name(wave))
}

fn structural_event(persona: PersonaId, input: StructuralInput) -> StructuralPlan {
    StructuralPlan {
        event_id: structural_event_id(persona, input.wave, input.ordinal),
        wave: input.wave,
        ordinal: input.ordinal,
        kind: input.kind,
        source_id: input.source_id,
        parent_source_id: input.parent_source_id,
        source_scope_id: input.source_scope_id,
        destination_scope_id: input.destination_scope_id,
        child_variant: input.child_variant,
        depends_on: input.depends_on,
        paired_event_id: input.paired_event_id,
        history_neutral: true,
        requires_raw_only: input.requires_raw_only,
        transform: input.transform,
    }
}

fn traveler_variant(persona: PersonaId) -> FormatVariant {
    match persona {
        PersonaId::P02 => FormatVariant::Pcap,
        PersonaId::P03
        | PersonaId::P06
        | PersonaId::P07
        | PersonaId::P16
        | PersonaId::P17
        | PersonaId::P20 => FormatVariant::PdfScan,
        PersonaId::P04 | PersonaId::P05 | PersonaId::P10 | PersonaId::P14 | PersonaId::P18 => {
            FormatVariant::Xlsx
        }
        PersonaId::P01
        | PersonaId::P08
        | PersonaId::P09
        | PersonaId::P11
        | PersonaId::P12
        | PersonaId::P13
        | PersonaId::P15
        | PersonaId::P19 => FormatVariant::Docx,
    }
}

fn scope_id_for_path(person: &PersonPlan, path: &str) -> Result<String, PersonaPlanError> {
    let matches = person
        .scopes
        .iter()
        .filter(|scope| scope.path == path)
        .map(|scope| scope.id.clone())
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(PersonaPlanError::Invalid(format!(
            "structural scope path is not unique: {path}"
        )));
    }
    Ok(matches[0].clone())
}

fn choose_scope(
    person: &PersonPlan,
    preferred_paths: &[&str],
    excluded: &BTreeSet<String>,
) -> Result<String, PersonaPlanError> {
    for path in preferred_paths {
        let scope = scope_id_for_path(person, path)?;
        if !excluded.contains(&scope) {
            return Ok(scope);
        }
    }
    person
        .scopes
        .iter()
        .map(|scope| scope.id.clone())
        .find(|scope| !excluded.contains(scope))
        .ok_or_else(|| PersonaPlanError::Invalid("no structural destination scope".into()))
}

fn build_structural_events(
    person: &PersonPlan,
    profile: PersonaProfile,
) -> Result<Vec<StructuralPlan>, PersonaPlanError> {
    let sources = source_projections(person)?;
    let mut rename_sources = Vec::new();
    if profile == PersonaProfile::Full {
        for scope in &person.scopes {
            let source = sources
                .iter()
                .filter(|source| {
                    source.scope_id == scope.id
                        && source.cohort == Some(Cohort::U)
                        && source.gate_role == GateRole::ContractContributor
                })
                .min_by_key(|source| source.ordinal)
                .ok_or_else(|| {
                    PersonaPlanError::Invalid(format!(
                        "full structural rename lacks a U source in {}",
                        scope.id
                    ))
                })?;
            rename_sources.push(source.clone());
        }
    } else {
        rename_sources.push(
            sources
                .iter()
                .filter(|source| {
                    source.cohort == Some(Cohort::U)
                        && source.gate_role == GateRole::ContractContributor
                })
                .min_by_key(|source| source.ordinal)
                .cloned()
                .ok_or_else(|| PersonaPlanError::Invalid("no structural U source".into()))?,
        );
    }
    let primary = rename_sources
        .first()
        .ok_or_else(|| PersonaPlanError::Invalid("no primary structural source".into()))?;
    let archive_scope = scope_id_for_path(person, "archive/closed")?;
    let traveler = sources
        .iter()
        .filter(|source| {
            source.variant == traveler_variant(person.id)
                && source.gate_role == GateRole::RawOnly
                && source.planned_chunks == 0
                && source.scope_id != archive_scope
        })
        .min_by_key(|source| source.ordinal)
        .ok_or_else(|| PersonaPlanError::Invalid("no persona raw-only traveler".into()))?;
    let png = sources
        .iter()
        .filter(|source| {
            source.variant == FormatVariant::Png
                && source.gate_role == GateRole::RawOnly
                && source.planned_chunks == 0
        })
        .take(2)
        .collect::<Vec<_>>();
    if png.len() != 2 || png[0].source_id == png[1].source_id {
        return Err(PersonaPlanError::Invalid(
            "structural transforms need two distinct PNG parents".into(),
        ));
    }
    let create_scope = scope_id_for_path(person, "downloads/inbox")?;
    let restore_scope = choose_scope(
        person,
        &["documents/reference", "desktop/working", "cloud/my-files"],
        &BTreeSet::from([create_scope.clone()]),
    )?;
    let traveler_w1_scope = choose_scope(
        person,
        &["desktop/working", "downloads/inbox", "cloud/my-files"],
        &BTreeSet::from([traveler.scope_id.clone(), archive_scope.clone()]),
    )?;
    let traveler_w2_scope = choose_scope(
        person,
        &[
            "cloud/team-shared",
            "downloads/exports",
            "documents/reference",
        ],
        &BTreeSet::from([
            traveler.scope_id.clone(),
            traveler_w1_scope.clone(),
            archive_scope.clone(),
        ]),
    )?;
    let replacement_count = person
        .cohorts
        .iter()
        .filter(|cohort| matches!(cohort.cohort, Cohort::P | Cohort::X))
        .map(|cohort| cohort.source_count)
        .sum::<u32>();
    let first_new = person.raw_files + replacement_count + 1;
    if first_new + 2 > 999_999 {
        return Err(PersonaPlanError::Invalid(
            "structural source namespace is exhausted".into(),
        ));
    }
    let new_ids = (0..3)
        .map(|offset| format!("{}-src-{:06}", person.id.as_str(), first_new + offset))
        .collect::<Vec<_>>();

    let w1_rename_id = structural_event_id(person.id, Wave::W1, 1);
    let w1_move_id = structural_event_id(person.id, Wave::W1, 2);
    let w1_create_id = structural_event_id(person.id, Wave::W1, 3);
    let w4_delete_id = structural_event_id(person.id, Wave::W4, 2);
    let w5_restore_id = structural_event_id(person.id, Wave::W5, 1);
    let mut events = vec![
        structural_event(
            person.id,
            StructuralInput {
                wave: Wave::W1,
                ordinal: 1,
                kind: StructuralKind::SameScopeRename,
                source_id: primary.source_id.clone(),
                parent_source_id: None,
                source_scope_id: Some(primary.scope_id.clone()),
                destination_scope_id: Some(primary.scope_id.clone()),
                child_variant: None,
                depends_on: Vec::new(),
                paired_event_id: None,
                requires_raw_only: false,
                transform: TransformKind::CanonicalSource,
            },
        ),
        structural_event(
            person.id,
            StructuralInput {
                wave: Wave::W1,
                ordinal: 2,
                kind: StructuralKind::CrossScopeMove,
                source_id: traveler.source_id.clone(),
                parent_source_id: None,
                source_scope_id: Some(traveler.scope_id.clone()),
                destination_scope_id: Some(traveler_w1_scope.clone()),
                child_variant: None,
                depends_on: Vec::new(),
                paired_event_id: None,
                requires_raw_only: true,
                transform: TransformKind::CanonicalSource,
            },
        ),
        structural_event(
            person.id,
            StructuralInput {
                wave: Wave::W1,
                ordinal: 3,
                kind: StructuralKind::Create,
                source_id: new_ids[0].clone(),
                parent_source_id: None,
                source_scope_id: None,
                destination_scope_id: Some(create_scope.clone()),
                child_variant: Some(traveler.variant),
                depends_on: Vec::new(),
                paired_event_id: None,
                requires_raw_only: true,
                transform: TransformKind::CanonicalSource,
            },
        ),
    ];
    for (index, source) in rename_sources.iter().enumerate() {
        events.push(structural_event(
            person.id,
            StructuralInput {
                wave: Wave::W2,
                ordinal: u8::try_from(index + 1).map_err(|_| {
                    PersonaPlanError::Invalid("too many structural rename events".into())
                })?,
                kind: StructuralKind::SameScopeRename,
                source_id: source.source_id.clone(),
                parent_source_id: None,
                source_scope_id: Some(source.scope_id.clone()),
                destination_scope_id: Some(source.scope_id.clone()),
                child_variant: None,
                depends_on: (index == 0)
                    .then(|| w1_rename_id.clone())
                    .into_iter()
                    .collect(),
                paired_event_id: None,
                requires_raw_only: false,
                transform: TransformKind::CanonicalSource,
            },
        ));
    }
    let w2_move_ordinal = u8::try_from(rename_sources.len() + 1)
        .map_err(|_| PersonaPlanError::Invalid("too many W2 structural events".into()))?;
    let primary_w2_id = structural_event_id(person.id, Wave::W2, 1);
    let w2_move_id = structural_event_id(person.id, Wave::W2, w2_move_ordinal);
    events.push(structural_event(
        person.id,
        StructuralInput {
            wave: Wave::W2,
            ordinal: w2_move_ordinal,
            kind: StructuralKind::CrossScopeMove,
            source_id: traveler.source_id.clone(),
            parent_source_id: None,
            source_scope_id: Some(traveler_w1_scope),
            destination_scope_id: Some(traveler_w2_scope.clone()),
            child_variant: None,
            depends_on: vec![w1_move_id],
            paired_event_id: None,
            requires_raw_only: true,
            transform: TransformKind::CanonicalSource,
        },
    ));
    events.extend([
        structural_event(
            person.id,
            StructuralInput {
                wave: Wave::W3,
                ordinal: 1,
                kind: StructuralKind::ExactDuplicate,
                source_id: primary.source_id.clone(),
                parent_source_id: None,
                source_scope_id: Some(primary.scope_id.clone()),
                destination_scope_id: Some(primary.scope_id.clone()),
                child_variant: None,
                depends_on: vec![primary_w2_id],
                paired_event_id: None,
                requires_raw_only: false,
                transform: TransformKind::CanonicalSource,
            },
        ),
        structural_event(
            person.id,
            StructuralInput {
                wave: Wave::W3,
                ordinal: 2,
                kind: StructuralKind::NearDuplicate,
                source_id: new_ids[1].clone(),
                parent_source_id: Some(png[0].source_id.clone()),
                source_scope_id: None,
                destination_scope_id: Some(png[0].scope_id.clone()),
                child_variant: Some(FormatVariant::Png),
                depends_on: Vec::new(),
                paired_event_id: None,
                requires_raw_only: true,
                transform: TransformKind::NearPngOneChannel,
            },
        ),
        structural_event(
            person.id,
            StructuralInput {
                wave: Wave::W3,
                ordinal: 3,
                kind: StructuralKind::DerivedFormat,
                source_id: new_ids[2].clone(),
                parent_source_id: Some(png[1].source_id.clone()),
                source_scope_id: None,
                destination_scope_id: Some(png[1].scope_id.clone()),
                child_variant: Some(FormatVariant::PdfScan),
                depends_on: Vec::new(),
                paired_event_id: None,
                requires_raw_only: true,
                transform: TransformKind::PngToScanPdf,
            },
        ),
        structural_event(
            person.id,
            StructuralInput {
                wave: Wave::W4,
                ordinal: 1,
                kind: StructuralKind::ArchiveMove,
                source_id: traveler.source_id.clone(),
                parent_source_id: None,
                source_scope_id: Some(traveler_w2_scope),
                destination_scope_id: Some(archive_scope),
                child_variant: None,
                depends_on: vec![w2_move_id],
                paired_event_id: None,
                requires_raw_only: true,
                transform: TransformKind::CanonicalSource,
            },
        ),
        structural_event(
            person.id,
            StructuralInput {
                wave: Wave::W4,
                ordinal: 2,
                kind: StructuralKind::DeleteForRestore,
                source_id: new_ids[0].clone(),
                parent_source_id: None,
                source_scope_id: Some(create_scope.clone()),
                destination_scope_id: None,
                child_variant: None,
                depends_on: vec![w1_create_id],
                paired_event_id: Some(w5_restore_id.clone()),
                requires_raw_only: true,
                transform: TransformKind::CanonicalSource,
            },
        ),
        structural_event(
            person.id,
            StructuralInput {
                wave: Wave::W5,
                ordinal: 1,
                kind: StructuralKind::RestoreToActiveScope,
                source_id: new_ids[0].clone(),
                parent_source_id: None,
                source_scope_id: None,
                destination_scope_id: Some(restore_scope),
                child_variant: None,
                depends_on: vec![w4_delete_id.clone()],
                paired_event_id: Some(w4_delete_id),
                requires_raw_only: true,
                transform: TransformKind::CanonicalSource,
            },
        ),
    ]);
    Ok(events)
}

/// Expand at most one person's bounded source inventory at runtime.  The
/// expansion is deliberately not represented in the serialized plan.
pub fn source_projections(person: &PersonPlan) -> Result<Vec<SourceProjection>, PersonaPlanError> {
    if person.raw_files > 16_000 {
        return Err(PersonaPlanError::Invalid(
            "source expansion exceeds per-person bound".into(),
        ));
    }
    let variant_slots = fair_slots(
        &person
            .variants
            .iter()
            .map(|row| row.files)
            .collect::<Vec<_>>(),
        person.raw_files,
    )?;
    let contributor_positions: Vec<_> = variant_slots
        .iter()
        .enumerate()
        .filter_map(|(ordinal, variant)| {
            (person.variants[*variant].gate_role == GateRole::ContractContributor)
                .then_some(ordinal)
        })
        .collect();
    let minima: Vec<_> = person
        .scopes
        .iter()
        .map(|scope| scope.current_chunks.div_ceil(72))
        .collect();
    if contributor_positions.len()
        < checked_sum(minima.iter().copied(), "scope contributor minima")? as usize
    {
        return Err(PersonaPlanError::Invalid(
            "not enough contributors for scope chunk ceilings".into(),
        ));
    }
    let mut remaining: Vec<_> = person.scopes.iter().map(|row| row.raw_files).collect();
    let mut scope_slots = vec![usize::MAX; person.raw_files as usize];
    let mut assignment = 0;
    for (scope, minimum) in minima.into_iter().enumerate() {
        if remaining[scope] < minimum {
            return Err(PersonaPlanError::Invalid(
                "scope raw-file capacity is below its contributor minimum".into(),
            ));
        }
        for _ in 0..minimum {
            let ordinal = contributor_positions[assignment];
            scope_slots[ordinal] = scope;
            remaining[scope] -= 1;
            assignment += 1;
        }
    }
    let tail = fair_slots(&remaining, person.raw_files - assignment as u32)?;
    for (ordinal, scope) in scope_slots
        .iter_mut()
        .filter(|slot| **slot == usize::MAX)
        .zip(tail)
    {
        *ordinal = scope;
    }
    let contributors: Vec<_> = person
        .scopes
        .iter()
        .enumerate()
        .map(|(scope, _)| {
            contributor_positions
                .iter()
                .filter(|ordinal| scope_slots[**ordinal] == scope)
                .count() as u32
        })
        .collect();
    for (scope, contributor_count) in person.scopes.iter().zip(&contributors) {
        if *contributor_count == 0
            || scope.current_chunks < *contributor_count
            || scope.current_chunks > contributor_count.saturating_mul(72)
        {
            return Err(PersonaPlanError::Invalid(
                "scope contributor bounds cannot satisfy its chunk target".into(),
            ));
        }
    }
    let cohort_counts = person
        .cohorts
        .iter()
        .map(|row| row.source_count)
        .collect::<Vec<_>>();
    let contributor_total = contributor_positions.len() as u32;
    if checked_sum(cohort_counts.iter().copied(), "cohort source counts")? != contributor_total
        || person.cohorts.iter().any(|row| {
            row.chunks < row.source_count || row.chunks > row.source_count.saturating_mul(72)
        })
    {
        return Err(PersonaPlanError::Invalid(
            "cohort source/chunk bounds are infeasible".into(),
        ));
    }

    let cohort_count_matrix = balanced_count_matrix(
        &contributors,
        &cohort_counts,
        &person
            .scopes
            .iter()
            .map(|row| row.current_chunks)
            .collect::<Vec<_>>(),
        &person
            .cohorts
            .iter()
            .map(|row| row.chunks)
            .collect::<Vec<_>>(),
        person.current_chunks == 120_000,
    )?;
    let mut cohort_by_ordinal = vec![None; person.raw_files as usize];
    for scope_index in 0..person.scopes.len() {
        let ordinals = contributor_positions
            .iter()
            .copied()
            .filter(|ordinal| scope_slots[*ordinal] == scope_index)
            .collect::<Vec<_>>();
        let cohort_slots =
            fair_slots(&cohort_count_matrix[scope_index], contributors[scope_index])?;
        for (ordinal, cohort) in ordinals.into_iter().zip(cohort_slots) {
            cohort_by_ordinal[ordinal] = Some(cohort);
        }
    }

    let mut positions = vec![vec![Vec::<usize>::new(); person.cohorts.len()]; person.scopes.len()];
    for ordinal in &contributor_positions {
        let cohort = cohort_by_ordinal[*ordinal]
            .ok_or_else(|| PersonaPlanError::Invalid("contributor has no history cohort".into()))?;
        positions[scope_slots[*ordinal]][cohort].push(*ordinal);
    }
    let row_remaining = person
        .scopes
        .iter()
        .enumerate()
        .map(|(scope, row)| row.current_chunks - contributors[scope])
        .collect::<Vec<_>>();
    let col_remaining = person
        .cohorts
        .iter()
        .map(|row| row.chunks - row.source_count)
        .collect::<Vec<_>>();
    let capacities = positions
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| (cell.len() as u32).saturating_mul(71))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let extras = transport_extras(&row_remaining, &col_remaining, &capacities)?;
    let mut planned_by_ordinal = vec![0u32; person.raw_files as usize];
    for scope in 0..person.scopes.len() {
        for cohort in 0..person.cohorts.len() {
            let cell = &positions[scope][cohort];
            if cell.is_empty() {
                if extras[scope][cohort] != 0 {
                    return Err(PersonaPlanError::Invalid(
                        "chunk flow entered an empty cohort/scope cell".into(),
                    ));
                }
                continue;
            }
            let values = h(
                cell.len() as u32 + extras[scope][cohort],
                &vec![1; cell.len()],
            );
            for (ordinal, chunks) in cell.iter().zip(values) {
                if !(1..=72).contains(&chunks) {
                    return Err(PersonaPlanError::Invalid(
                        "contributor density exceeds 72 chunks per source".into(),
                    ));
                }
                planned_by_ordinal[*ordinal] = chunks;
            }
        }
    }

    let rows = variant_slots
        .into_iter()
        .enumerate()
        .map(|(ordinal, variant_index)| {
            let scope_index = scope_slots[ordinal];
            let variant = &person.variants[variant_index];
            SourceProjection {
                source_id: format!("{}-src-{:06}", person.id.as_str(), ordinal + 1),
                ordinal: ordinal as u32,
                scope_id: person.scopes[scope_index].id.clone(),
                variant: variant.variant,
                gate_role: variant.gate_role,
                disposition: variant.disposition,
                planned_chunks: planned_by_ordinal[ordinal],
                cohort: cohort_by_ordinal[ordinal].map(|index| person.cohorts[index].cohort),
            }
        })
        .collect::<Vec<_>>();
    let projected_cohort_chunks = person
        .cohorts
        .iter()
        .map(|cohort| {
            rows.iter()
                .filter(|row| row.cohort == Some(cohort.cohort))
                .map(|row| row.planned_chunks)
                .sum::<u32>()
        })
        .collect::<Vec<_>>();
    if rows.iter().map(|row| row.planned_chunks).sum::<u32>() != person.current_chunks
        || projected_cohort_chunks
            != person
                .cohorts
                .iter()
                .map(|row| row.chunks)
                .collect::<Vec<_>>()
    {
        return Err(PersonaPlanError::Invalid(
            "source projection marginals differ".into(),
        ));
    }
    Ok(rows)
}

fn transport_extras(
    row_demands: &[u32],
    column_demands: &[u32],
    cell_capacities: &[Vec<u32>],
) -> Result<Vec<Vec<u32>>, PersonaPlanError> {
    let (sent, result) = transport_extras_partial(row_demands, column_demands, cell_capacities)?;
    if sent != checked_sum(row_demands.iter().copied(), "transport row demands")? {
        return Err(PersonaPlanError::Invalid(
            "whole-source cohort chunk targets are infeasible".into(),
        ));
    }
    Ok(result)
}

fn transport_extras_partial(
    row_demands: &[u32],
    column_demands: &[u32],
    cell_capacities: &[Vec<u32>],
) -> Result<(u32, Vec<Vec<u32>>), PersonaPlanError> {
    let row_total = checked_sum(row_demands.iter().copied(), "transport row demands")?;
    let column_total = checked_sum(column_demands.iter().copied(), "transport column demands")?;
    if cell_capacities.len() != row_demands.len()
        || cell_capacities
            .iter()
            .any(|row| row.len() != column_demands.len())
        || row_total != column_total
    {
        return Err(PersonaPlanError::Invalid(
            "cohort chunk transportation dimensions are invalid".into(),
        ));
    }
    let row_count = row_demands.len();
    let column_count = column_demands.len();
    let source = 0usize;
    let row_start = 1usize;
    let column_start = row_start + row_count;
    let sink = column_start + column_count;
    let node_count = sink + 1;
    let mut residual = vec![vec![0u32; node_count]; node_count];
    for (row, demand) in row_demands.iter().enumerate() {
        residual[source][row_start + row] = *demand;
        for (column, capacity) in cell_capacities[row].iter().enumerate() {
            residual[row_start + row][column_start + column] = *capacity;
        }
    }
    for (column, demand) in column_demands.iter().enumerate() {
        residual[column_start + column][sink] = *demand;
    }

    let mut sent = 0u32;
    loop {
        let mut parent = vec![None; node_count];
        parent[source] = Some(source);
        let mut queue = VecDeque::from([source]);
        while let Some(node) = queue.pop_front() {
            for next in 0..node_count {
                if residual[node][next] > 0 && parent[next].is_none() {
                    parent[next] = Some(node);
                    queue.push_back(next);
                }
            }
        }
        if parent[sink].is_none() {
            break;
        }
        let mut amount = u32::MAX;
        let mut node = sink;
        while node != source {
            let previous = parent[node].expect("reachable flow node has a parent");
            amount = amount.min(residual[previous][node]);
            node = previous;
        }
        node = sink;
        while node != source {
            let previous = parent[node].expect("reachable flow node has a parent");
            residual[previous][node] -= amount;
            residual[node][previous] += amount;
            node = previous;
        }
        sent += amount;
    }
    let result = (0..row_count)
        .map(|row| {
            (0..column_count)
                .map(|column| {
                    cell_capacities[row][column] - residual[row_start + row][column_start + column]
                })
                .collect()
        })
        .collect();
    Ok((sent, result))
}

fn balanced_count_matrix(
    row_counts: &[u32],
    column_counts: &[u32],
    row_chunks: &[u32],
    column_chunks: &[u32],
    require_history_scope_coverage: bool,
) -> Result<Vec<Vec<u32>>, PersonaPlanError> {
    let total = checked_sum(row_counts.iter().copied(), "scope contributor counts")?;
    let total_chunks = checked_sum(row_chunks.iter().copied(), "scope chunk targets")?;
    if total == 0
        || total_chunks == 0
        || total != checked_sum(column_counts.iter().copied(), "cohort contributor counts")?
        || row_counts.len() != row_chunks.len()
        || column_counts.len() != column_chunks.len()
        || total_chunks != checked_sum(column_chunks.iter().copied(), "cohort chunk targets")?
    {
        return Err(PersonaPlanError::Invalid(
            "cohort source transportation dimensions are invalid".into(),
        ));
    }
    let mut matrix = vec![vec![0u32; column_counts.len()]; row_counts.len()];
    if require_history_scope_coverage {
        if column_counts.len() < 4
            || row_counts.len() != SCOPES_PER_PERSON
            || row_counts.iter().any(|count| *count < 4)
            || column_counts[..4]
                .iter()
                .any(|count| *count < row_counts.len() as u32)
        {
            return Err(PersonaPlanError::Invalid(
                "full-profile history cohorts cannot cover every scope".into(),
            ));
        }
        for row in &mut matrix {
            for value in &mut row[..4] {
                *value = 1;
            }
        }
    }
    let lower = matrix.clone();
    let row_lower = matrix
        .iter()
        .map(|row| row.iter().sum::<u32>())
        .collect::<Vec<_>>();
    let column_lower = (0..column_counts.len())
        .map(|column| matrix.iter().map(|row| row[column]).sum::<u32>())
        .collect::<Vec<_>>();
    if row_lower
        .iter()
        .zip(row_counts)
        .any(|(lower, target)| lower > target)
        || column_lower
            .iter()
            .zip(column_counts)
            .any(|(lower, target)| lower > target)
    {
        return Err(PersonaPlanError::Invalid(
            "cohort source lower bounds exceed their targets".into(),
        ));
    }
    let mut row_remaining = row_counts
        .iter()
        .zip(row_lower)
        .map(|(target, lower)| target - lower)
        .collect::<Vec<_>>();
    let mut column_remaining = column_counts
        .iter()
        .zip(column_lower)
        .map(|(target, lower)| target - lower)
        .collect::<Vec<_>>();
    while row_remaining.iter().sum::<u32>() != 0 {
        let mut best = None;
        for row in 0..row_counts.len() {
            if row_remaining[row] == 0 {
                continue;
            }
            for column in 0..column_counts.len() {
                if column_remaining[column] == 0 {
                    continue;
                }
                let source_denominator = i128::from(total);
                let chunk_denominator = i128::from(total_chunks) * i128::from(total_chunks);
                let desired_sources = i128::from(row_counts[row])
                    * i128::from(column_counts[column])
                    * chunk_denominator;
                let desired_chunks = i128::from(row_chunks[row])
                    * i128::from(column_chunks[column])
                    * source_denominator
                    * source_denominator;
                let selected =
                    i128::from(matrix[row][column]) * source_denominator * chunk_denominator;
                let deficit = desired_sources + desired_chunks - 2 * selected;
                if best.is_none_or(|(score, selected_row, selected_column)| {
                    deficit > score
                        || (deficit == score && (row, column) < (selected_row, selected_column))
                }) {
                    best = Some((deficit, row, column));
                }
            }
        }
        let (_, row, column) = best.ok_or_else(|| {
            PersonaPlanError::Invalid("cohort source scheduler exhausted early".into())
        })?;
        matrix[row][column] += 1;
        row_remaining[row] -= 1;
        column_remaining[column] -= 1;
    }
    if matrix
        .iter()
        .map(|row| row.iter().sum::<u32>())
        .ne(row_counts.iter().copied())
        || (0..column_counts.len())
            .map(|column| matrix.iter().map(|row| row[column]).sum::<u32>())
            .ne(column_counts.iter().copied())
        || (require_history_scope_coverage && matrix.iter().any(|row| row[..4].contains(&0)))
    {
        return Err(PersonaPlanError::Invalid(
            "cohort source transportation marginals differ".into(),
        ));
    }
    let required_flow = total_chunks - total;
    for _ in 0..128 {
        let current_flow = count_matrix_chunk_flow(
            &matrix,
            row_counts,
            column_counts,
            row_chunks,
            column_chunks,
        );
        if current_flow == required_flow {
            return Ok(matrix);
        }
        let mut best_swap = None;
        for first_row in 0..row_counts.len() {
            for second_row in (first_row + 1)..row_counts.len() {
                for first_column in 0..column_counts.len() {
                    for second_column in 0..column_counts.len() {
                        if first_column == second_column
                            || matrix[first_row][first_column] == lower[first_row][first_column]
                            || matrix[second_row][second_column] == lower[second_row][second_column]
                        {
                            continue;
                        }
                        matrix[first_row][first_column] -= 1;
                        matrix[second_row][second_column] -= 1;
                        matrix[first_row][second_column] += 1;
                        matrix[second_row][first_column] += 1;
                        let flow = count_matrix_chunk_flow(
                            &matrix,
                            row_counts,
                            column_counts,
                            row_chunks,
                            column_chunks,
                        );
                        matrix[first_row][first_column] += 1;
                        matrix[second_row][second_column] += 1;
                        matrix[first_row][second_column] -= 1;
                        matrix[second_row][first_column] -= 1;
                        if flow > current_flow
                            && best_swap.is_none_or(|(best_flow, _, _, _, _)| flow > best_flow)
                        {
                            best_swap =
                                Some((flow, first_row, second_row, first_column, second_column));
                        }
                    }
                }
            }
        }
        let Some((_, first_row, second_row, first_column, second_column)) = best_swap else {
            break;
        };
        matrix[first_row][first_column] -= 1;
        matrix[second_row][second_column] -= 1;
        matrix[first_row][second_column] += 1;
        matrix[second_row][first_column] += 1;
    }
    Err(PersonaPlanError::Invalid(
        "whole-source cohort chunk targets are infeasible".into(),
    ))
}

fn count_matrix_chunk_flow(
    matrix: &[Vec<u32>],
    row_counts: &[u32],
    column_counts: &[u32],
    row_chunks: &[u32],
    column_chunks: &[u32],
) -> u32 {
    let row_extras = row_chunks
        .iter()
        .zip(row_counts)
        .map(|(chunks, sources)| chunks - sources)
        .collect::<Vec<_>>();
    let column_extras = column_chunks
        .iter()
        .zip(column_counts)
        .map(|(chunks, sources)| chunks - sources)
        .collect::<Vec<_>>();
    let capacities = matrix
        .iter()
        .map(|row| {
            row.iter()
                .map(|sources| sources.saturating_mul(71))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    transport_extras_partial(&row_extras, &column_extras, &capacities)
        .map(|(sent, _)| sent)
        .unwrap_or(0)
}

fn fair_slots(counts: &[u32], total: u32) -> Result<Vec<usize>, PersonaPlanError> {
    if checked_sum(counts.iter().copied(), "slot counts")? != total {
        return Err(PersonaPlanError::Invalid(
            "slot counts do not sum to total".into(),
        ));
    }
    let mut used = vec![0u32; counts.len()];
    let mut result = Vec::with_capacity(total as usize);
    for step in 0..total {
        let mut best = None;
        for (index, count) in counts.iter().enumerate() {
            if used[index] == *count {
                continue;
            }
            let deficit = (u64::from(step + 1) * u64::from(*count)) as i64
                - (u64::from(total) * u64::from(used[index])) as i64;
            if best.is_none_or(|(score, chosen): (i64, usize)| {
                deficit > score || (deficit == score && index < chosen)
            }) {
                best = Some((deficit, index));
            }
        }
        let index = best
            .ok_or_else(|| PersonaPlanError::Invalid("slot scheduler exhausted early".into()))?
            .1;
        used[index] += 1;
        result.push(index);
    }
    Ok(result)
}

fn portable_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains("//")
        && path.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && component.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'-' | b'_')
                })
        })
}

fn checked_sum(
    values: impl IntoIterator<Item = u32>,
    label: &str,
) -> Result<u32, PersonaPlanError> {
    values.into_iter().try_fold(0u32, |total, value| {
        total.checked_add(value).ok_or_else(|| {
            PersonaPlanError::Invalid(format!("{label} exceeds the supported count range"))
        })
    })
}

impl PersonaPlan {
    pub fn validate(&self) -> Result<(), PersonaPlanError> {
        let e = frozen_plan(self.profile);
        if self.schema != SCHEMA || self.fixture_id != FIXTURE_ID || self.seed != SEED {
            return Err(PersonaPlanError::Invalid("identity".into()));
        }
        if self.personas.len() != 20 {
            return Err(PersonaPlanError::Invalid("persona count".into()));
        }
        let mut ids = BTreeSet::new();
        let structural_count = if self.profile == PersonaProfile::Full {
            30
        } else {
            11
        };
        for p in &self.personas {
            if !ids.insert(p.id)
                || p.formats.len() != 14
                || p.variants.len() != 22
                || p.scopes.len() != 20
                || p.cohorts.len() != 5
                || p.structural.len() != structural_count
                || p.waves.len() != 6
            {
                return Err(PersonaPlanError::Invalid("duplicate or cardinality".into()));
            }
            if checked_sum(
                p.formats.iter().map(|f| u32::from(f.percentage)),
                "family percentages",
            )? != 100
                || checked_sum(p.formats.iter().map(|f| f.files), "family files")? != p.raw_files
                || checked_sum(p.variants.iter().map(|v| v.files), "variant files")? != p.raw_files
                || checked_sum(p.scopes.iter().map(|s| s.raw_files), "scope files")? != p.raw_files
                || checked_sum(p.scopes.iter().map(|s| s.current_chunks), "scope chunks")?
                    != p.current_chunks
                || checked_sum(p.cohorts.iter().map(|c| c.chunks), "cohort chunks")?
                    != p.current_chunks
            {
                return Err(PersonaPlanError::Invalid("marginals".into()));
            }
            let mut scope_ids = BTreeSet::new();
            let mut scope_paths = BTreeSet::new();
            if p.scopes.iter().any(|scope| {
                !portable_path(&scope.path)
                    || !scope_ids.insert(scope.id.as_str())
                    || !scope_paths.insert(scope.path.to_ascii_lowercase())
            }) {
                return Err(PersonaPlanError::Invalid(
                    "scope identities or portable paths are invalid".into(),
                ));
            }
            source_projections(p).map_err(|error| {
                PersonaPlanError::Invalid(format!("{} source projection: {error}", p.id.as_str()))
            })?;
            if p.structural != build_structural_events(p, self.profile)? {
                return Err(PersonaPlanError::Invalid(format!(
                    "{} structural allocation differs from its plan-owned projection",
                    p.id.as_str()
                )));
            }
        }
        if *self != e {
            return Err(PersonaPlanError::Invalid("not canonical rebuild".into()));
        }
        Ok(())
    }
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, PersonaPlanError> {
        self.validate()?;
        let mut b = canonical_json_bytes(
            &serde_json::to_value(self).map_err(|e| PersonaPlanError::Serialize(e.to_string()))?,
        )
        .map_err(|e| PersonaPlanError::Serialize(e.to_string()))?;
        b.push(b'\n');
        Ok(b)
    }
    pub fn digest(&self) -> Result<String, PersonaPlanError> {
        Ok(hash_bytes(&self.canonical_bytes()?))
    }
    pub fn parse_canonical(b: &[u8]) -> Result<Self, PersonaPlanError> {
        preflight_json(b)?;
        let p: Self =
            serde_json::from_slice(b).map_err(|e| PersonaPlanError::Json(e.to_string()))?;
        if p.canonical_bytes()? != b {
            return Err(PersonaPlanError::NonCanonical);
        }
        Ok(p)
    }
}

fn bounded_vec<'de, D, T, const MAX: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const MAX: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for Bounded<T, MAX> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "at most {MAX} entries")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX));
            while let Some(value) = seq.next_element()? {
                if out.len() == MAX {
                    return Err(serde::de::Error::custom("array capacity"));
                }
                out.push(value);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_seq(Bounded::<T, MAX>(std::marker::PhantomData))
}
fn bounded_5<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    bounded_vec::<D, T, 5>(d)
}
fn bounded_6<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    bounded_vec::<D, T, 6>(d)
}
fn bounded_14<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    bounded_vec::<D, T, 14>(d)
}
fn bounded_16<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    bounded_vec::<D, T, 16>(d)
}
fn bounded_20<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    bounded_vec::<D, T, 20>(d)
}
fn bounded_22<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    bounded_vec::<D, T, 22>(d)
}
fn bounded_30<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    bounded_vec::<D, T, 30>(d)
}

fn preflight_json(bytes: &[u8]) -> Result<(), PersonaPlanError> {
    if bytes.len() > MAX_CANONICAL_BYTES {
        return Err(PersonaPlanError::Invalid("plan byte bound".into()));
    }
    let (mut depth, mut strings, mut escaped, mut in_string, mut tokens) =
        (0usize, 0usize, false, false, 0usize);
    for &byte in bytes {
        if in_string {
            strings = strings
                .checked_add(1)
                .ok_or_else(|| PersonaPlanError::Invalid("plan string".into()))?;
            if strings > 8192 {
                return Err(PersonaPlanError::Invalid("plan string bound".into()));
            }
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                strings = 0;
            }
            b'{' | b'[' => {
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| PersonaPlanError::Invalid("plan depth".into()))?;
                tokens = tokens
                    .checked_add(1)
                    .ok_or_else(|| PersonaPlanError::Invalid("plan token".into()))?;
            }
            b'}' | b']' | b',' | b':' => {
                tokens = tokens
                    .checked_add(1)
                    .ok_or_else(|| PersonaPlanError::Invalid("plan token".into()))?;
                depth = if matches!(byte, b'}' | b']') {
                    depth
                        .checked_sub(1)
                        .ok_or_else(|| PersonaPlanError::Invalid("plan structure".into()))?
                } else {
                    depth
                };
            }
            _ => {}
        }
        if depth > 64 || tokens > 100_000 {
            return Err(PersonaPlanError::Invalid("plan lexical bound".into()));
        }
    }
    if in_string || depth != 0 {
        return Err(PersonaPlanError::Invalid("plan structure".into()));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_are_deterministic_and_semantically_complete() {
        for q in [
            PersonaProfile::Tiny,
            PersonaProfile::Pilot,
            PersonaProfile::Full,
        ] {
            let p = frozen_plan(q);
            p.validate().unwrap();
            for person in &p.personas {
                let sources = source_projections(person).unwrap();
                assert_eq!(sources.len() as u32, person.raw_files);
                assert!(sources.iter().all(|row| {
                    if row.gate_role == GateRole::ContractContributor {
                        (1..=72).contains(&row.planned_chunks) && row.cohort.is_some()
                    } else {
                        row.planned_chunks == 0 && row.cohort.is_none()
                    }
                }));
                assert_eq!(
                    sources.iter().map(|row| row.planned_chunks).sum::<u32>(),
                    person.current_chunks
                );
                for scope in &person.scopes {
                    let rows = sources
                        .iter()
                        .filter(|row| row.scope_id == scope.id)
                        .collect::<Vec<_>>();
                    assert_eq!(rows.len() as u32, scope.raw_files);
                    assert_eq!(
                        rows.iter().map(|row| row.planned_chunks).sum::<u32>(),
                        scope.current_chunks
                    );
                    assert!(
                        rows.iter()
                            .any(|row| { row.gate_role == GateRole::ContractContributor })
                    );
                }
                for family in FormatFamily::ALL {
                    assert_eq!(
                        person
                            .variants
                            .iter()
                            .filter(|row| row.family == family)
                            .map(|row| u32::from(row.percentage))
                            .sum::<u32>(),
                        100
                    );
                }
                assert_eq!(
                    person
                        .cohorts
                        .iter()
                        .map(|cohort| cohort.source_count)
                        .sum::<u32>(),
                    sources
                        .iter()
                        .filter(|row| row.gate_role == GateRole::ContractContributor)
                        .count() as u32
                );
                for cohort in &person.cohorts {
                    let selected = sources
                        .iter()
                        .filter(|row| row.cohort == Some(cohort.cohort))
                        .collect::<Vec<_>>();
                    assert_eq!(selected.len() as u32, cohort.source_count);
                    assert_eq!(
                        selected.iter().map(|row| row.planned_chunks).sum::<u32>(),
                        cohort.chunks
                    );
                    if q == PersonaProfile::Full && cohort.cohort != Cohort::U {
                        assert_eq!(
                            selected
                                .iter()
                                .map(|row| row.scope_id.as_str())
                                .collect::<BTreeSet<_>>()
                                .len(),
                            SCOPES_PER_PERSON
                        );
                    }
                }
                assert_structural_events(person, q, &sources);
                assert_eq!(
                    &person.scopes[..12]
                        .iter()
                        .map(|scope| scope.path.as_str())
                        .collect::<Vec<_>>(),
                    &primary_paths(person.id).to_vec()
                );
            }
            assert_eq!(
                p.digest().unwrap(),
                match q {
                    PersonaProfile::Tiny => TINY_PLAN_HASH,
                    PersonaProfile::Pilot => PILOT_PLAN_HASH,
                    PersonaProfile::Full => FULL_PLAN_HASH,
                }
            );
            assert_eq!(
                p.personas.iter().map(|x| x.raw_files).sum::<u32>(),
                match q {
                    PersonaProfile::Tiny => 4000,
                    PersonaProfile::Pilot => 20000,
                    PersonaProfile::Full => 195000,
                }
            );
            assert_eq!(
                p.personas.iter().map(|x| x.scopes.len()).sum::<usize>(),
                400
            );
            assert_eq!(
                p.personas.iter().map(|x| x.current_chunks).sum::<u32>(),
                match q {
                    PersonaProfile::Tiny => 4_131,
                    PersonaProfile::Pilot => 240_000,
                    PersonaProfile::Full => 2_400_000,
                }
            );
        }
    }

    #[test]
    fn malformed_scope_capacity_is_rejected_without_panicking() {
        let mut plan = frozen_plan(PersonaProfile::Tiny);
        let person = &mut plan.personas[0];
        let moved = person.scopes[0].raw_files;
        person.scopes[0].raw_files = 0;
        person.scopes[1].raw_files += moved;
        let result = std::panic::catch_unwind(|| plan.validate());
        assert!(matches!(result, Ok(Err(_))));

        let mut overflow = frozen_plan(PersonaProfile::Tiny);
        overflow.personas[0].scopes[0].current_chunks = u32::MAX;
        overflow.personas[0].scopes[1].current_chunks = u32::MAX;
        let result = std::panic::catch_unwind(|| overflow.validate());
        assert!(matches!(result, Ok(Err(_))));
        let mut bytes = canonical_json_bytes(&serde_json::to_value(&overflow).unwrap()).unwrap();
        bytes.push(b'\n');
        let result = std::panic::catch_unwind(|| PersonaPlan::parse_canonical(&bytes));
        assert!(matches!(result, Ok(Err(_))));
    }

    fn assert_structural_events(
        person: &PersonPlan,
        profile: PersonaProfile,
        sources: &[SourceProjection],
    ) {
        let source_by_id = sources
            .iter()
            .map(|source| (source.source_id.as_str(), source))
            .collect::<std::collections::BTreeMap<_, _>>();
        let expected = if profile == PersonaProfile::Full {
            [3, 21, 3, 2, 1]
        } else {
            [3, 2, 3, 2, 1]
        };
        assert_eq!(person.structural.len(), expected.iter().sum::<usize>());
        let mut event_ids = BTreeSet::new();
        let mut seen_ids = BTreeSet::new();
        for (wave, count) in [Wave::W1, Wave::W2, Wave::W3, Wave::W4, Wave::W5]
            .into_iter()
            .zip(expected)
        {
            let wave_events = person
                .structural
                .iter()
                .filter(|event| event.wave == wave)
                .collect::<Vec<_>>();
            assert_eq!(wave_events.len(), count);
            assert_eq!(
                wave_events
                    .iter()
                    .map(|event| event.ordinal)
                    .collect::<Vec<_>>(),
                (1..=u8::try_from(count).unwrap()).collect::<Vec<_>>()
            );
        }
        for event in &person.structural {
            assert!(event_ids.insert(event.event_id.as_str()));
            assert!(
                event
                    .depends_on
                    .iter()
                    .all(|dependency| seen_ids.contains(dependency.as_str()))
            );
            seen_ids.insert(event.event_id.as_str());
        }
        let near = person
            .structural
            .iter()
            .find(|event| event.kind == StructuralKind::NearDuplicate)
            .unwrap();
        let derive = person
            .structural
            .iter()
            .find(|event| event.kind == StructuralKind::DerivedFormat)
            .unwrap();
        assert_eq!(near.transform, TransformKind::NearPngOneChannel);
        assert_eq!(near.child_variant, Some(FormatVariant::Png));
        assert_eq!(derive.transform, TransformKind::PngToScanPdf);
        assert_eq!(derive.child_variant, Some(FormatVariant::PdfScan));
        assert_ne!(near.parent_source_id, derive.parent_source_id);
        for parent in [&near.parent_source_id, &derive.parent_source_id] {
            let source = source_by_id[parent.as_deref().unwrap()];
            assert_eq!(source.variant, FormatVariant::Png);
            assert_eq!(source.gate_role, GateRole::RawOnly);
        }
        let rename_scopes = person
            .structural
            .iter()
            .filter(|event| event.wave == Wave::W2 && event.kind == StructuralKind::SameScopeRename)
            .map(|event| event.source_scope_id.as_deref().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            rename_scopes.len(),
            if profile == PersonaProfile::Full {
                SCOPES_PER_PERSON
            } else {
                1
            }
        );
        let moves = person
            .structural
            .iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    StructuralKind::CrossScopeMove | StructuralKind::ArchiveMove
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(moves.len(), 3);
        assert!(
            moves
                .iter()
                .all(|event| event.source_id == moves[0].source_id)
        );
        let create = person
            .structural
            .iter()
            .find(|event| event.kind == StructuralKind::Create)
            .unwrap();
        let delete = person
            .structural
            .iter()
            .find(|event| event.kind == StructuralKind::DeleteForRestore)
            .unwrap();
        let restore = person
            .structural
            .iter()
            .find(|event| event.kind == StructuralKind::RestoreToActiveScope)
            .unwrap();
        assert_eq!(create.source_id, delete.source_id);
        assert_eq!(delete.source_id, restore.source_id);
        assert_eq!(delete.paired_event_id.as_deref(), Some(&*restore.event_id));
        assert_eq!(restore.paired_event_id.as_deref(), Some(&*delete.event_id));
    }

    #[test]
    fn strict_parser_rejects_legacy_unknown_noncanonical_and_mutated_plans() {
        let p = frozen_plan(PersonaProfile::Tiny);
        let b = p.canonical_bytes().unwrap();
        assert_eq!(PersonaPlan::parse_canonical(&b).unwrap(), p);
        assert!(PersonaPlan::parse_canonical(&b[..b.len() - 1]).is_err());

        let mut v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("root".into(), serde_json::json!("/tmp"));
        assert!(serde_json::from_value::<PersonaPlan>(v).is_err());

        let mut legacy = p.clone();
        legacy.schema = "kio.persona.plan/v1".into();
        assert!(legacy.validate().is_err());
        let mut reordered = p.clone();
        reordered.personas.swap(0, 1);
        assert!(reordered.validate().is_err());
        let mut duplicate = p.clone();
        duplicate.personas[1].id = PersonaId::P01;
        assert!(duplicate.validate().is_err());
        let mut escaped = p.clone();
        escaped.personas[0].scopes[0].path = "../victim".into();
        assert!(escaped.validate().is_err());
        let mut count = p.clone();
        count.personas[0].raw_files += 1;
        assert!(count.validate().is_err());

        let text = std::str::from_utf8(&b).unwrap();
        let duplicate_key = text.replacen('{', "{\"schema\":\"kio.persona.plan/v2\",", 1);
        assert!(PersonaPlan::parse_canonical(duplicate_key.as_bytes()).is_err());
    }

    #[test]
    fn parser_preflight_and_bounded_vectors_reject_floods() {
        let escaped = format!("\"{}\"", "\\\\".repeat(8192));
        assert!(preflight_json(escaped.as_bytes()).is_err());
        let plan = frozen_plan(PersonaProfile::Tiny);
        let mut value = serde_json::to_value(plan).unwrap();
        let people = value["personas"].as_array_mut().unwrap();
        let duplicate = people[0].clone();
        people.push(duplicate);
        assert!(serde_json::from_value::<PersonaPlan>(value).is_err());
        assert!(PersonaPlan::parse_canonical(&vec![b' '; MAX_CANONICAL_BYTES + 1]).is_err());
    }
}
