//! Standalone stdio MCP host, with no display or hardware required.
fn main() {
    let mut project = None;
    let mut audible = false;
    for arg in std::env::args_os().skip(1) {
        if arg == "--audio" {
            audible = true;
        } else if arg == "--help" || arg == "-h" {
            eprintln!(
                "Usage: peel-mcp [--audio] [project.peelproj]\nMCP over stdio. Silent by default. --audio opens hardware output.\nFor the desktop UI, use peachey-studio --mcp [project.peelproj]."
            );
            return;
        } else if arg.to_string_lossy().starts_with('-') || project.is_some() {
            eprintln!("Unknown argument: {}", arg.to_string_lossy());
            std::process::exit(2);
        } else {
            project = Some(std::path::PathBuf::from(arg));
        }
    }
    if let Err(error) = peel_mcp::run_headless(project, audible) {
        eprintln!("Peel MCP: {error}");
        std::process::exit(1);
    }
}
