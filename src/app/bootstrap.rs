use std::sync::Arc;

use crate::memory::WorkspaceMemory;
use crate::rag::VectorStore;
use crate::tools::{
    BashTool, ExecTool, PatchFileTool, RagInsertTool, RagSearchTool, ReadFileTool, ReadMemoryTool,
    SendFileTool, TavilySearchTool, Tool, WaitTool, WebFetchTool, WriteFileTool, WriteMemoryTool,
};

pub struct AppBootstrap {
    pub tools: Vec<Arc<dyn Tool>>,
}

pub fn build_app_bootstrap() -> Result<AppBootstrap, Box<dyn std::error::Error>> {
    let vector_store = Arc::new(VectorStore::new()?);
    let workspace_memory = Arc::new(WorkspaceMemory::new("."));
    let tavily_key = std::env::var("TAVILY_API_KEY").unwrap_or_default();

    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(BashTool::new()),
        Arc::new(ExecTool),
        Arc::new(WaitTool),
        Arc::new(WriteFileTool),
        Arc::new(ReadFileTool),
        Arc::new(PatchFileTool),
        Arc::new(TavilySearchTool::new(tavily_key)),
        Arc::new(WebFetchTool::new()),
        Arc::new(RagSearchTool::new(vector_store.clone())),
        Arc::new(RagInsertTool::new(vector_store.clone())),
        Arc::new(ReadMemoryTool::new(workspace_memory.clone())),
        Arc::new(WriteMemoryTool::new(workspace_memory.clone())),
        Arc::new(SendFileTool),
    ];

    let lazy_lsp = Arc::new(crate::lsp_client::LazyLspClient::new(
        std::env::current_dir()?,
    ));
    tools.push(Arc::new(crate::tools::LspGotoDefinitionTool {
        lsp_client: lazy_lsp.clone(),
    }));
    tools.push(Arc::new(crate::tools::LspFindReferencesTool {
        lsp_client: lazy_lsp.clone(),
    }));
    tools.push(Arc::new(crate::tools::LspHoverTool {
        lsp_client: lazy_lsp.clone(),
    }));
    tools.push(Arc::new(crate::tools::LspGetDiagnosticsTool {
        lsp_client: lazy_lsp.clone(),
    }));
    tools.push(Arc::new(crate::tools::LspGetSymbolsTool {
        lsp_client: lazy_lsp,
    }));

    if let Ok(token) = std::env::var("TELEGRAM_BOT_TOKEN") {
        tools.push(Arc::new(crate::tools::SendTelegramMessageTool::new(token)));
    }

    Ok(AppBootstrap { tools })
}
