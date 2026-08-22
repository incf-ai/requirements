use ron;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde_with::skip_serializing_none]
struct RequirementDefinitionV1 {
    title: String,
    test: Option<TestReferenceKind>,
    tests: Option<nunny::Vec<TestReferenceKind>>,
    dependency: Option<DependencyReferenceKind>,
    dependencies: Option<nunny::Vec<DependencyReferenceKind>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
enum TestReferenceKind {
    TestReferenceV1(LocalGitReference),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct LocalGitReference {
    path: String,
    commit: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
enum DependencyReferenceKind {
    RequirementReferenceV1(LocalGitReference),
}

#[cfg(test)]
mod test {
    use crate::disk::requirement::RequirementDefinitionV1;

    #[test]
    fn test() -> Result<(), Box<dyn std::error::Error>> {
        // Read the RON file contents into a string
        let file_path = "toplevel/requirements/definition/requirement.ron";
        let ron_data = std::fs::read_to_string(file_path)?;

        // Configure options to support the extensions
        let options = ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::EXPLICIT_STRUCT_NAMES)
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
            .with_default_extension(ron::extensions::Extensions::UNWRAP_NEWTYPES)
            .with_default_extension(ron::extensions::Extensions::UNWRAP_VARIANT_NEWTYPES);

        // Deserialize the data from the file string
        let config: RequirementDefinitionV1 = options.from_str(&ron_data)?;

        println!(
            "Successfully deserialized from '{}':\n{:#?}",
            file_path, config
        );

        Ok(())
    }
}
