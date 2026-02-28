use std::fs;
use std::path::Path;

fn main() {
    let dist = Path::new("../../web/dist");
    if !dist.exists() {
        fs::create_dir_all(dist).expect("Failed to create web/dist");
        fs::write(
            dist.join("index.html"),
            "<html><body><h1>Placeholder</h1><p>Run <code>cd web &amp;&amp; bun install &amp;&amp; bun run build</code></p></body></html>",
        )
        .expect("Failed to write placeholder index.html");
    }
}
