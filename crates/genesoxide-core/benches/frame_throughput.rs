use divan::Bencher;
use genesoxide_core::{Command, GenesisCore};

fn main() {
    divan::main();
}

#[divan::bench]
fn step_frame_empty_rom(bencher: Bencher) {
    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(vec![0; 1024]));

    bencher.bench_local(|| {
        core.execute(Command::StepFrame);
    });
}
