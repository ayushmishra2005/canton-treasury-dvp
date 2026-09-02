use anyhow::Result;
use canton_treasury_dvp_bridge::probe;

fn main() -> Result<()> {
    let report = probe::run()?;
    for line in &report.lines {
        println!("{line}");
    }
    if !report.passed {
        std::process::exit(1);
    }
    Ok(())
}
