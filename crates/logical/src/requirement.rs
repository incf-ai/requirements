use non_empty_string::NonEmptyString;

struct Requirement {
    title: String,
    tests: Vec<TestKind>,
    requirement_text: Option<NonEmptyString>,
    requirement_guidance: Option<NonEmptyString>,
    test_guidance: Option<NonEmptyString>,
    dependencies: Vec<DependencyKind>,
    results: Vec<TestResult>
}

enum TestKind {
    Manual(ManualTest),
    Automatic,
}

struct ManualTest {
    title: String,
    result_kind: ResultKind
}

enum ResultKind {
    FreeForm {
        template: Option<FreeFormTemplate>
    },
    Template
}

struct FreeFormTemplate {
    files: Vec<TemplateFile>
}

struct TemplateFile {
    name: String,
    content: Vec<u8>
}

enum DependencyKind {
    Submodules,
    Specific,
    Git,
}

struct TestResult {
    title: String,
    path: String,
    commit: String,
    status: Option<String>,
}