use duct::Expression;
use eyre::Result;
use eyre::WrapErr;
use serde_derive::Deserialize;

use crate::config::Config;
use crate::output::Output;
use crate::toolset::ToolsetBuilder;

/// [internal] This is an internal command that writes an envrc file
/// for direnv to consume.
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, hide = true)]
pub struct DirenvExec {}

#[derive(Debug, Default, Deserialize)]
struct DirenvWatches {
    #[serde(rename(deserialize = "DIRENV_WATCHES"))]
    watches: String,
}

impl DirenvExec {
    pub fn run(self, mut config: Config, out: &mut Output) -> Result<()> {
        let ts = ToolsetBuilder::new().build(&mut config)?;

        let mut cmd = env_cmd();

        for (k, v) in ts.env_with_path(&config) {
            cmd = cmd.env(k, v);
        }

        let json = cmd!("direnv", "watch", "json", ".tool-versions")
            .read()
            .wrap_err("error running direnv watch")?;
        let w: DirenvWatches = serde_json::from_str(&json)?;
        cmd = cmd.env("DIRENV_WATCHES", w.watches);

        rtxprint!(out, "{}", cmd.read()?);
        Ok(())
    }
}

#[cfg(test)]
fn env_cmd() -> Expression {
    cmd!("env")
}

#[cfg(not(test))]
fn env_cmd() -> Expression {
    cmd!("direnv", "dump")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_str_eq;

    use crate::assert_cli;
    use crate::cli::tests::grep;

    #[test]
    fn test_direnv_exec() {
        let stdout = assert_cli!("direnv", "exec");
        assert_str_eq!(grep(stdout, "JDXCODE_TINY="), "JDXCODE_TINY=3.1.0");
    }
}
