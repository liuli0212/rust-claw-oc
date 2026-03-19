use super::protocol::{clean_schema, Tool, ToolError};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub struct LspGotoDefinitionTool {
    pub lsp_client: std::sync::Arc<crate::lsp_client::LazyLspClient>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LspGotoDefinitionArgs {
    pub path: String,
    pub line: u32,
    pub character: u32,
}

#[async_trait]
impl Tool for LspGotoDefinitionTool {
    fn name(&self) -> String {
        "lsp_goto_definition".to_string()
    }

    fn description(&self) -> String {
        "Go to definition of a symbol using rust-analyzer.".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(LspGotoDefinitionArgs)).unwrap())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        let parsed: LspGotoDefinitionArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let path = std::path::PathBuf::from(&parsed.path);
        let client = self
            .lsp_client
            .get_client()
            .await
            .map_err(ToolError::ExecutionFailed)?;
        let result = client
            .goto_definition(path, parsed.line, parsed.character)
            .await
            .map_err(ToolError::ExecutionFailed)?;

        Ok(serde_json::to_string_pretty(&result).unwrap())
    }
}

pub struct LspFindReferencesTool {
    pub lsp_client: std::sync::Arc<crate::lsp_client::LazyLspClient>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LspFindReferencesArgs {
    pub path: String,
    pub line: u32,
    pub character: u32,
    pub include_declaration: bool,
}

#[async_trait]
impl Tool for LspFindReferencesTool {
    fn name(&self) -> String {
        "lsp_find_references".to_string()
    }

    fn description(&self) -> String {
        "Find all references to a symbol using rust-analyzer.".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(LspFindReferencesArgs)).unwrap())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        let parsed: LspFindReferencesArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let path = std::path::PathBuf::from(&parsed.path);
        let client = self
            .lsp_client
            .get_client()
            .await
            .map_err(ToolError::ExecutionFailed)?;
        let result = client
            .find_references(
                path,
                parsed.line,
                parsed.character,
                parsed.include_declaration,
            )
            .await
            .map_err(ToolError::ExecutionFailed)?;

        Ok(serde_json::to_string_pretty(&result).unwrap())
    }
}

pub struct LspHoverTool {
    pub lsp_client: std::sync::Arc<crate::lsp_client::LazyLspClient>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LspHoverArgs {
    pub path: String,
    pub line: u32,
    pub character: u32,
}

#[async_trait]
impl Tool for LspHoverTool {
    fn name(&self) -> String {
        "lsp_hover".to_string()
    }

    fn description(&self) -> String {
        "Get hover information (types, docs) for a symbol using rust-analyzer.".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(LspHoverArgs)).unwrap())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        let parsed: LspHoverArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let path = std::path::PathBuf::from(&parsed.path);
        let client = self
            .lsp_client
            .get_client()
            .await
            .map_err(ToolError::ExecutionFailed)?;
        let result = client
            .hover(path, parsed.line, parsed.character)
            .await
            .map_err(ToolError::ExecutionFailed)?;

        Ok(serde_json::to_string_pretty(&result).unwrap())
    }
}

pub struct LspGetDiagnosticsTool {
    pub lsp_client: std::sync::Arc<crate::lsp_client::LazyLspClient>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LspGetDiagnosticsArgs {
    pub path: String,
}

#[async_trait]
impl Tool for LspGetDiagnosticsTool {
    fn name(&self) -> String {
        "lsp_get_diagnostics".to_string()
    }

    fn description(&self) -> String {
        "Get compilation errors and warnings for a file from rust-analyzer.".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(LspGetDiagnosticsArgs)).unwrap())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        let parsed: LspGetDiagnosticsArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let path = std::path::PathBuf::from(&parsed.path);
        let client = self
            .lsp_client
            .get_client()
            .await
            .map_err(ToolError::ExecutionFailed)?;
        let result = client
            .get_diagnostics(path)
            .await
            .map_err(ToolError::ExecutionFailed)?;

        Ok(serde_json::to_string_pretty(&result).unwrap())
    }
}

pub struct LspGetSymbolsTool {
    pub lsp_client: std::sync::Arc<crate::lsp_client::LazyLspClient>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LspGetSymbolsArgs {
    pub path: String,
}

#[async_trait]
impl Tool for LspGetSymbolsTool {
    fn name(&self) -> String {
        "lsp_get_symbols".to_string()
    }

    fn description(&self) -> String {
        "Get all symbols (structs, enums, functions) in a file using rust-analyzer.".to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        clean_schema(serde_json::to_value(schemars::schema_for!(LspGetSymbolsArgs)).unwrap())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<String, crate::tools::ToolError> {
        let parsed: LspGetSymbolsArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let path = std::path::PathBuf::from(&parsed.path);
        let client = self
            .lsp_client
            .get_client()
            .await
            .map_err(ToolError::ExecutionFailed)?;
        let result = client
            .document_symbols(path)
            .await
            .map_err(ToolError::ExecutionFailed)?;

        Ok(serde_json::to_string_pretty(&result).unwrap())
    }
}
