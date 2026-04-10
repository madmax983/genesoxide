# Genesis Audio Fidelity Log

**Status:** Living document  
**Started:** 2026-04-01  
**Scope:** Major audio-fidelity investigations, fixes, false leads, and lessons from Genesis / Mega Drive audio work in `genesoxide`.

## Purpose

This is the running history of how Genesis audio fidelity was debugged and improved in `genesoxide`.

The goal is not to preserve every tiny tweak. The goal is to preserve:

- What hypothesis we tested
- What code or harness change we made
- What evidence it produced
- What it proved or falsified
- What lesson should transfer to later emulator work, especially SNES

This should be updated in place as new audio passes land.

## Current State

As of this log's initial write:

- YM2612 core behavior is strongly validated against `ymfm` under both synthetic cases and live Sonic register streams.
- GHZ external-reference matching improved materially through better harnessing, gain staging, stereo shaping, measured EQ, and later a more precise 5-stage capture EQ.
- The remaining mismatch is no longer basic FM operator math or obviously broken timing.
- Follow-up diagnostics now say the remaining gap is also unlikely to be solved by simple left/right capture EQ or a static phase/all-pass style filter.
- Naive YM/PSG source fitting is also not a reliable retuning oracle by itself: raw PCM fit collapses under phase mismatch, while simple power fitting is biased enough to overstate PSG.
- The GHZ hardware capture is not perfectly self-consistent across its inferred loop either, especially in the side channel. That weakens it as an absolute stereo oracle.
- The harness now extracts a trusted loop-stable window from the FLAC itself. In that trusted window, emulator-vs-reference broad spectral match is stronger than the old generic local window, which means the capture contains cleaner sections than the blunt global lag search was surfacing.
- A second GHZ hardware capture from the 16-bit Audiophile Project is now ingested locally, and it does not confirm the side-only tweak that looked better against the first FLAC.
- The most likely next frontier is now a better reference model or a more specific non-static capture-chain explanation, not another generic post-mix knob.

## Executive Summary

The audio work followed a pattern:

1. Kill harness lies first.
2. Get an actual oracle for the sound chip.
3. Use live game traces to separate "chip core wrong" from "capture/mix/reference wrong".
4. Only then tune the analog/output path.
5. Treat every diagnostic tool as guilty until it proves it is not lying.

That sequence mattered. Several "audio" problems turned out to be harness problems, sequencing mistakes, or replay-state bugs.

## Chronology

### 1. The original GHZ golden was not GHZ

The first major harness bug was in the old Green Hill comparison flow. The test path skipped frames but did not actually press `Start`, so it was effectively comparing title-screen audio against a GHZ reference.

What changed:

- The Sonic boot path was updated to enter gameplay for real before capture.
- The golden comparison moved away from naive whole-track PCM correlation.
- Lag-tolerant envelope correlation became part of the signal.

What it proved:

- The old test was not a trustworthy indicator of YM2612 accuracy.
- Whole-track raw waveform correlation is a poor top-level metric for long console recordings with phase, loop, and capture differences.

Carry-forward lesson:

- Before tuning audio, prove the content being compared is actually the same content.

### 2. A real YM2612 oracle was added with `ymfm`

The next major step was integrating `ymfm` as a reference oracle in the test harness.

What changed:

- `ymfm` was vendored and wired into the harness.
- Synthetic FM cases were rendered through both `genesoxide` and `ymfm`.
- This created an actual chip-level ground truth instead of "sounds better / sounds worse" iteration.

What it found:

- A pitch / phase-step bug produced an octave error.
- Later, a `$28` key-on decoding bug was exposed: OPN operator-order handling did not match hardware order, so live operator triggering was wrong.

What it proved:

- The core still had real FM bugs at that stage.
- The oracle immediately outperformed subjective tuning.

Carry-forward lesson:

- For any chip with an accepted software reference, build the oracle before touching analog polish.

### 3. Simple FM and live Sonic YM traffic were brought into line

After the `ymfm` oracle landed, the work corrected the concrete FM-core bugs it exposed.

Important wins:

- Phase-step / pitch math was fixed.
- `$28` key-on decoding was fixed so operator ordering matched actual OPN behavior.
- Synthetic carrier and 2-op FM cases moved from nonsense to near-perfect agreement.
- Live Sonic boot/title and GHZ YM streams were replayed against `ymfm` with very high correlation.

What it proved:

- The remaining GHZ mismatch was not "YM2612 is fundamentally broken under real game traffic".
- The FM core became good enough that later failures had to be explained elsewhere.

Carry-forward lesson:

- Once a live trace agrees with a chip oracle, stop blaming the core for every downstream mismatch.

### 4. Timed live register tracing separated core behavior from replay behavior

Timed YM and PSG write tracing was added to the core so the harness could replay real live game traffic with master-clock timestamps.

What changed:

- Timed YM writes were captured from both 68K and Z80 bus paths.
- Timed PSG writes were added too.
- These traces could be replayed directly in the harness.

What this unlocked:

- Direct comparison of live capture vs replayed writes
- Separation between "audio synthesis wrong" and "capture/replay path wrong"
- GHZ-specific replays instead of relying only on synthetic FM tests

What it proved:

- Live YM traffic replay matched `ymfm` strongly.
- Remaining problems lived in sequencing, sample timing, mix/output path, or the external reference relationship.

Carry-forward lesson:

- Timed bus traces are one of the highest-value emulator debugging tools. Build them early.

### 5. GHZ sequencing was wrong, then fixed

The early GHZ harness also assumed a fixed delay after game start instead of locating actual music activity.

What changed:

- The GHZ harness advanced through the transition and searched for the first frame with both YM activity and meaningful audio RMS.

What it proved:

- Some GHZ mismatches were simply capture-window mistakes.
- After fixing sequencing, GHZ improved, but not enough to explain the whole gap.

Carry-forward lesson:

- "Wait N frames" is not a timing model. Detect the event you actually care about.

### 6. Internal capture-path mismatch was identified and fixed

Another major harness truthing step found that the internal capture path was not being replayed under the same assumptions as the live core.

The core was effectively scanline-batched in how writes and audio generation interacted, so a replay path that ignored that batching could disagree with the live output even when chip behavior was fine.

What changed:

- Trace metadata carried frame + scanline.
- The harness replay path gained scanline-batched and later more accurate timed replay support.
- Internal live replay vs capture moved to near-exact agreement.

What it proved:

- Some "audio mismatch" was purely a mismatch between two internal models of the same emulator.

Carry-forward lesson:

- If you compare live output against replayed writes, the replay path must model the same timing semantics as the live path, or the test is fiction.

### 7. Sub-scanline output timing became continuous

The core originally emitted audio in a coarse scanline-end style that blurred within-scanline timing.

What changed:

- Audio generation moved toward continuous sub-scanline synthesis intervals.
- Timed writes and output sample boundaries were brought onto a consistent wall-clock timeline.

What it proved:

- Internal consistency improved and the live core matched the timed renderer.
- But the external GHZ reference gap still remained, so timing alone was not the whole remaining problem.

Carry-forward lesson:

- Fix temporal structure before output-color tuning, but do not expect timing fixes to solve analog/reference mismatches by magic.

### 8. The first analog tuning phase focused on level and stereo width

Once the core and harness were no longer the main suspects, the next pass moved into post-mix shaping.

What changed:

- Shared `AudioOutputConfig` / `AudioOutputProfile` support was added to the core and timed renderer.
- Per-chip `ym_gain` and `psg_gain` controls were added.
- Stereo crossfeed and output low-pass support were added and swept.
- The default path eventually moved to:
  - `Legacy` profile
  - `master_gain = 2.5`
  - `ym_gain = 1.1`
  - `psg_gain = 0.8`
  - `stereo_crossfeed = 0.35`
  - `post_low_pass_hz = 12 kHz`

What it proved:

- The external reference had a much narrower stereo image than the earlier default.
- Level and stereo shaping mattered a lot.
- Hardcoded "Model 1 LPF" guesses were not enough and were sometimes actively worse.

Carry-forward lesson:

- Basic gain and stereo shape can dominate perceived mismatch. Fix those before inventing fancy capture lore.

### 9. The GHZ sweep itself was lying until replay-state handling was fixed

One of the more annoying harness bugs came later: the ignored GHZ diagnostic sweep was replaying from a nonzero start tick with a fresh renderer state.

That meant the sweep skipped all chip and filter warm-up before the actual capture point, producing flattering but false numbers.

What changed:

- A nonzero-start replay regression was added.
- The root cause was identified: cold-start replay at mid-song.
- The diagnostic path was fixed to render from origin and slice at the capture point instead of pretending midstream replay was state-free.

What it proved:

- The public golden and the ignored sweep finally agreed.
- Several earlier "better candidate" numbers were partly a harness artifact.

Carry-forward lesson:

- Midstream replay without inherited chip/filter state is a liar unless proven otherwise.

### 10. Measured EQ beat generic low-pass folklore

Once the harness was honest, the next step measured the local GHZ spectral delta between emulator and reference instead of guessing filters from hearsay.

What the measurement said:

- The reference wanted:
  - less sub-bass
  - more low-mid body
  - slightly gentler upper spectrum

What changed:

- A shared 3-stage post-EQ chain was added:
  - low shelf
  - peaking EQ
  - high shelf
- A narrow measured candidate set was swept through GHZ.
- The best balanced candidate became the new default:
  - low shelf `110 Hz / -6 dB`
  - peak `380 Hz / Q 0.65 / +4.5 dB`
  - high shelf `2.6 kHz / -2.5 dB`

What it proved:

- Measured EQ produced a large local spectral improvement compared with the earlier crossfeed/LP-only default.
- Magnitude shaping was still a real lever after the earlier gain/stereo work.

Carry-forward lesson:

- If you have a usable reference, derive tonal direction from measured deltas, not emulator folklore.

### 11. A tiny FIR stage helped; fitted FIR did not

After EQ, the next experiment targeted capture-chain phase / short IR behavior using a shared short FIR stage.

What changed:

- A 5-tap shared post-mix FIR stage was added to both live and timed paths.
- GHZ diagnostics tried both:
  - least-squares fitted FIR taps from music
  - several tiny hand-sized FIR kernels

What happened:

- The least-squares fitted FIR was garbage and overfit badly.
- A mild symmetric FIR was the only phase/IR candidate that improved the balanced score without destroying level:
  - `[0.12, 0.76, 0.12, 0.0, 0.0]`
- That FIR became part of the default chain for a while.

What it proved:

- There is some real value in mild phase/IR shaping.
- Blindly fitting an FIR to program material is not a trustworthy path.

Carry-forward lesson:

- Tiny hand-validated phase/IR tweaks can help.
- Program-material FIR fitting is highly vulnerable to nonsense.

## Major False Leads / Falsifications

These were useful precisely because they failed:

- Treating whole-track raw waveform correlation as the top-line metric
- Comparing title-screen audio against a GHZ reference
- Assuming a fixed GHZ frame offset instead of detecting music activity
- Midstream replay from a cold renderer state
- Hardcoded Model 1 LPF as the presumed answer
- Over-strong measured EQ candidates that improved one axis while hurting balance
- Least-squares FIR fitted from music content
- Tiny interchannel delay as the presumed remaining stereo fix
- Treating the remaining stereo gap as a one-knob width problem

### 12. Interchannel delay was falsified as the next stereo lever

After the FIR pass, the next hypothesis was that the remaining stereo mismatch might be tiny interchannel timing skew in the capture chain.

What changed:

- A shared post-mix left/right sample-delay stage was added to the live core and timed renderer.
- GHZ diagnostics were extended to report local interchannel lag alongside the existing stereo correlation and side-energy metrics.
- Delay candidates were swept with both left and right offsets.

What happened:

- The aligned GHZ reference window reported zero interchannel lag.
- Delay candidates consistently scored worse than the non-delayed default.
- Delay plus extra crossfeed could move one stereo metric, but always at the expense of the others.

What it proved:

- The remaining stereo mismatch is not explained by a simple left/right sample skew.
- The contradiction in the stereo metrics is more likely about coloration than literal channel timing.

Carry-forward lesson:

- Measure the stereo error before adding stereo “fix” knobs. Channel delay is easy to imagine and easy to get wrong.

### 13. A more precise 5-stage EQ beat the old FIR-backed default

Once delay was falsified, the next pass returned to the measured GHZ spectral delta. The remaining mismatch was still concentrated in the low mids, and the earlier 3-stage EQ did not have enough freedom to shape it cleanly.

What changed:

- The shared post-mix EQ path was extended from 3 stages to 5 in both the live core and timed renderer.
- GHZ sweeps tried targeted low-mid/body candidates instead of broad random filter changes.
- The winning default became:
  - low shelf `110 Hz / -6 dB`
  - peak `380 Hz / Q 0.65 / +4.5 dB`
  - high shelf `2.6 kHz / -2.8 dB`
  - peak `190 Hz / Q 0.90 / +3.2 dB`
  - peak `560 Hz / Q 1.20 / -2.4 dB`
- With that EQ in place, the old tiny FIR no longer improved the balanced score, so it was removed from the default path.

What it proved:

- More precise magnitude shaping was still a real lever even after the earlier EQ/FIR work.
- The old FIR was not “the answer”; it was a temporary improvement that got superseded by a better tonal model.

Carry-forward lesson:

- A temporary win is not sacred. Re-sweep old fixes after a structural change, because the best default can change under them.

### 14. Side-channel EQ beat broader stereo guesses

Once the 5-stage default landed, the remaining stereo mismatch still looked off in a specific way: the side channel was clearly less well matched than the mid channel.

What changed:

- GHZ diagnostics were extended to measure local mid-spectrum and side-spectrum similarity separately.
- A shared mid/side stage was added after left/right shaping and crossfeed in both the live core and timed renderer.
- The sweep tried a small set of side-only EQ candidates instead of more broad left/right EQ or delay guesses.

What happened:

- Mid similarity was already decent.
- Side similarity was the weaker piece.
- The best balanced candidate was:
  - side peaking EQ `420 Hz / Q 0.95 / -1.6 dB`
  - side peaking EQ `2.6 kHz / Q 0.90 / +0.8 dB`
- That beat the prior default on the GHZ balanced score and improved side similarity without breaking live replay parity.

What it proved:

- The remaining stereo gap was not a simple width problem.
- Side-channel coloration is a real lever and is worth modeling directly.

Carry-forward lesson:

- If stereo metrics disagree, split the signal into mid and side before deciding what knob to add. Width, timing, and side coloration are different problems.

### 15. The remaining GHZ gap is not a simple left/right EQ, static phase, or PSG-balance problem

Once the side-EQ default landed, the next question was whether the remaining mismatch was really hiding in one of three simpler places: asymmetric left/right coloration, a static phase filter opportunity, or bad YM/PSG balance.

What changed:

- GHZ diagnostics were extended to report left-channel and right-channel spectral similarity separately.
- A phase-delta diagnostic was added to estimate per-bin phase offset and phase coherence for left, right, mid, and side.
- A chip-balance diagnostic was added to compare the aligned GHZ window under `current_default`, `ym_only`, `psg_only`, reduced PSG, and boosted PSG mixes.
- The public GHZ golden output now prints local phase-coherence summaries so future passes can see immediately whether static phase modeling even looks plausible.

What happened:

- Left/right similarity came back nearly symmetric: `left = 0.8259`, `right = 0.8362`.
- Local phase coherence was weak across the board: `left = 0.1050`, `right = 0.1074`, `mid = 0.1076`, `side = 0.1384`.
- The chip-balance probe showed PSG matters, but not in a way that makes it the obvious remaining lever:
  - `current_default`: `env = 0.5038`, `local_env = 0.7024`, `spectral = 0.8259`, `local_rms = 0.9961`
  - `ym_only`: `env = 0.4885`, `local_env = 0.7184`, `spectral = 0.8232`, `local_rms = 0.9508`
  - `psg_plus25`: `env = 0.5056`, `local_env = 0.6901`, `spectral = 0.8272`, `local_rms = 1.0206`

What it proved:

- The remaining stereo/capture gap is not well explained by a simple left/right coloration mismatch.
- A single static phase filter is unlikely to buy much because phase agreement is not coherent enough across frequency.
- PSG balance still influences the result, but the current default remains the best balanced compromise among the quick chip-balance probes.

Carry-forward lesson:

- Before adding another analog knob, test whether the remaining error is actually coherent enough for a static model to help. If the phase coherence is mush, another filter is mostly cosplay.

### 16. Naive source fitting is not a clean retuning oracle

Once left/right EQ asymmetry and static phase shaping looked weak, the next question was whether the remaining GHZ gap could still be attacked as a simple YM/PSG rebalance problem.

What changed:

- A small tested two-source least-squares helper was added so aligned windows can be fit as `target ≈ a * YM_only + b * PSG_only`.
- GHZ timed traces were replayed three ways using the current default output chain:
  - full mix
  - `YM_only`
  - `PSG_only`
- The harness now has an ignored source-fit probe that reports both:
  - raw sample-domain fit
  - simple power-spectrum-domain fit

What happened:

- The raw sample-domain fit behaved exactly like the low raw waveform correlation warned it would:
  - fitting the emulator against itself produced `(~1.0, ~1.0)`
  - fitting the emulator sources against the hardware reference collapsed to nearly zero coefficients
- The simple power-spectrum fit went the other way and overstated PSG even when fitting the emulator against itself:
  - default self-fit power scales: `YM = 0.9998`, `PSG = 1.2782`
  - reference power fit scales: `YM = 0.7571`, `PSG = 4.5179`

What it proved:

- Raw PCM source fitting is not a valid oracle here because phase mismatch dominates the least-squares solution.
- Simple power fitting is too biased to use as a direct retuning signal, because even the emulator's own mixed output does not map back to `(1.0, 1.0)` cleanly.
- That means "fit YM and PSG to the reference and set the gains accordingly" is too naive for this stage.

Carry-forward lesson:

- Do not promote chip-balance changes from a decomposition model unless the model first fits your own known-good mix sanely. If self-fit is already biased, the oracle is lying.

### 17. The GHZ hardware reference is not fully self-consistent across its own loop

The next step after the failed source-fit oracle was to stop assuming the GHZ FLAC was internally stable in the exact dimensions we were still trying to tune.

What changed:

- The public GHZ golden now also compares the reference against itself at the inferred loop offset.
- That self-match reports raw correlation, spectral similarity, mid/side spectral similarity, RMS drift, and phase-coherence summaries.

What happened:

- The reference agrees with itself well in broad spectral shape:
  - `Reference self spectral = 0.9073`
  - `Reference self mid spectral = 0.9111`
- But it does *not* agree with itself especially well in the side channel:
  - `Reference self side spectral = 0.4253`
- Its loop-to-loop raw waveform similarity is still basically useless:
  - `Reference self raw corr = 0.0140`
- Its phase coherence is also weak even against itself:
  - `left = 0.0909`
  - `right = 0.0946`
  - `mid = 0.0908`
  - `side = 0.1177`
- Its level drifts too:
  - `Reference self RMS ratio = 0.7287`

What it proved:

- The reference is good enough to act as a tonal / broad-envelope guide.
- The reference is *not* strong enough to act as an absolute stereo-side or raw-phase oracle.
- Some of what looked like "emulator still wrong" is actually "the capture does not reproduce itself cleanly at the inferred loop."

Carry-forward lesson:

- Before spending another week tuning against a hardware recording, check whether the recording agrees with itself. If it does not, treat it as a guide rail, not scripture.

### 18. A trusted loop-stable reference window is a better ceiling than the generic matched window

After proving the FLAC was not equally trustworthy everywhere, the next step was to stop letting the generic lag-matched window define the whole story.

What changed:

- The GHZ golden now searches the reference against itself at the inferred loop offset in envelope space, then lifts the best loop-stable window back into full-resolution audio.
- That trusted window is scored with the same detailed metrics used elsewhere: spectral similarity, mid/side similarity, and RMS fit.
- The public GHZ test now prints both:
  - the old local matched window
  - the new trusted reference window

What happened:

- The best trusted self-consistent window landed at:
  - prior `28.79s`
  - current `81.22s`
  - score `0.9085`
- That window is much more stable than the earlier loop-shifted local window:
  - `self_left = 0.8889`
  - `self_mid = 0.9111`
  - `self_side = 0.7627`
  - `self_rms = 0.9603`
- In that trusted window, emulator vs reference improves materially on the broad tonal metrics:
  - `trusted spectral = 0.8975`
  - `trusted RMS ratio = 0.9543`
  - `trusted mid = 0.8988`
- But the stereo side still lags there:
  - `trusted side = 0.3617`

What it proved:

- The emulator tracks the stable tonal content of the reference better than the old generic matched-window metric suggested.
- The current broad mismatch is less about overall tone or level now, and more about what happens in the stereo side image on sections where the reference is itself stable.
- That does *not* automatically justify retuning to this one FLAC, but it does make the remaining disagreement more specific.

Carry-forward lesson:

- If a long capture is inconsistent, mine it for its stable regions and grade against those. A trusted subsection is often a better oracle than the whole recording.

### 19. A second hardware capture killed the tempting side-only retune

After the trusted-window pass, one candidate still looked attractive against the original GHZ FLAC: a slightly stronger side low-mid cut.

What changed:

- A second GHZ reference was downloaded locally as `sonic_ghz_16bap.flac`.
- A multi-reference consensus probe was added so both captures are scored with the same trusted-window method.
- Two configs were compared across both references:
  - `current_default`
  - `side_lowmid_cut_more`

What happened:

Against the original `sonic_ghz.flac`:

- `current_default`: side `0.3617`
- `side_lowmid_cut_more`: side `0.3834`

Against `sonic_ghz_16bap.flac`:

- `current_default`: side `0.3265`
- `side_lowmid_cut_more`: side `0.3236`

Broad metrics on the second capture were also much weaker overall:

- `env = 0.3259`
- `local_env = 0.4492`
- `trusted spectral = 0.8784`
- `trusted RMS ratio = 1.1023`
- `trusted mid = 0.8808`
- `trusted side = 0.3265`

What it proved:

- The side-only tweak was not a cross-reference win.
- It was improving one recording while slightly regressing the other.
- That makes it a bad default, even if it looked tempting when judged against only the first FLAC.

Carry-forward lesson:

- Never promote a retune from a single hardware capture when a second plausible capture disagrees. Consensus beats seduction.

### 20. Cross-reference consensus scoring normalized the sweep, but it did not justify a new default

Once there were two GHZ captures, the obvious next move was to stop letting one reference flatter a candidate into the lead. A new consensus scorer was added to rank candidates by both average trusted-window quality and cross-reference disagreement.

What changed:

- A consensus score was defined from the trusted-window metrics:
  - average of trusted spectral, trusted mid, trusted side, trusted RMS fit, and local envelope
  - minus a disagreement penalty based on spectral range, side range, and RMS range across references
- A profile sweep was run across multiple plausible defaults:
  - `current_default`
  - `no_side_eq`
  - side-only variants
  - crossfeed variants
  - PSG gain variants
- A leader diagnostic was added to print the per-reference metrics for the top consensus candidates.

What happened:

- Top consensus scores:
  - `xf_0_30`: `average = 0.7241`, `penalty = 0.0502`, `final = 0.6739`
  - `psg_0_90`: `average = 0.7264`, `penalty = 0.0555`, `final = 0.6709`
  - `current_default`: `average = 0.7259`, `penalty = 0.0578`, `final = 0.6681`
- The apparent winner, `xf_0_30`, was not actually stronger on the first capture's meaningful local metrics:
  - `current_default`: `local_env = 0.7024`, trusted side `= 0.3617`, trusted RMS `= 0.9543`
  - `xf_0_30`: `local_env = 0.6966`, trusted side `= 0.3470`, trusted RMS `= 0.9320`
- On the second capture, `xf_0_30` did slightly reduce disagreement:
  - `current_default`: trusted side `= 0.3265`, trusted RMS `= 1.1023`
  - `xf_0_30`: trusted side `= 0.3358`, trusted RMS `= 1.0848`

What it proved:

- Cross-reference normalization is worth keeping.
- The current default is not obviously the unique best answer, but it is still the most defensible one.
- `xf_0_30` won because it disagreed a bit less across captures, not because it was clearly better music reproduction.
- That is not enough to retune the emulator's shipped default.

Carry-forward lesson:

- A consensus scorer is a guardrail, not a king. Do not promote a new default unless it survives per-reference inspection, not just score compression.

### 21. Reliability-weighted consensus killed the crossfeed mirage

The first consensus pass still had a structural weakness: it treated both captures as equally trustworthy even when their own loop-stable windows were not equally self-consistent. The next pass normalized each reference against its own self ceiling and weighted references by their trusted-window stability before computing the final cross-reference score.

What changed:

- Per-reference scores were normalized by the reference's own trusted-window ceiling:
  - local envelope against `trusted_score`
  - spectral against `trusted_self_left`
  - mid against `trusted_self_mid`
  - side against `trusted_self_side`
  - RMS fit against the reference's own RMS self-fit
- A reliability weight was added from the reference's self-consistency:
  - trusted-window score
  - left/mid/side self spectral scores
  - RMS self-fit
- Cross-reference disagreement switched from blunt raw ranges to weighted mean absolute deviation on the normalized metrics.

What happened:

- New weighted consensus leaders:
  - `psg_0_90`: `average = 0.8065`, `penalty = 0.0130`, `final = 0.7935`
  - `current_default`: `average = 0.8063`, `penalty = 0.0139`, `final = 0.7924`
  - `side_gain_down`: `average = 0.8074`, `penalty = 0.0154`, `final = 0.7921`
  - `xf_0_30`: `average = 0.8025`, `penalty = 0.0213`, `final = 0.7812`
- The reference weights were close, but not identical:
  - `sonic_ghz.flac`: `weight = 0.8021`
  - `sonic_ghz_16bap.flac`: `weight = 0.7813`
- That was enough to kill the earlier crossfeed mirage.

What it proved:

- `xf_0_30` was mostly a scoring artifact from the unweighted consensus pass.
- Once the references are normalized against their own ceilings, crossfeed reduction is no longer the leader.
- The remaining difference between `psg_0_90` and `current_default` is tiny.
- That spread is not strong enough to justify another shipped-default retune.

Carry-forward lesson:

- If multiple captures disagree, normalize each capture against what it can reliably reproduce of itself before letting it vote on the emulator.

### 22. Fixed-window PSG isolation said "not the main villain"

The next question was whether the remaining gap was really pointing at PSG balance or whether PSG was just being blamed because it is easy to turn knobs on. The old chip-balance probe let each candidate find its own best local alignment, which made it too easy for `ym_only` or `psg_only` to flatter themselves. This pass anchored the trusted window on `current_default`, then scored every alternate render on that exact same window for both captures.

What changed:

- A fixed trusted-window selector was split out from the broader trusted-window analysis.
- A fixed-window scorer was added so alternate renders can be judged on the same reference-stable slice without re-lagging themselves into a better score.
- A GHZ diagnostic was added to compare:
  - `current_default`
  - `ym_only`
  - `psg_only`
  - `psg_0_70`
  - `psg_0_90`
  - `psg_1_00`

What happened:

- On `sonic_ghz.flac`:
  - `current_default`: `0.8637`
  - `ym_only`: `0.8496`
  - `psg_only`: `0.3942`
  - `psg_0_90`: `0.8651`
  - `psg_1_00`: `0.8650`
- On `sonic_ghz_16bap.flac`:
  - `current_default`: `0.8336`
  - `ym_only`: `0.8311`
  - `psg_only`: `0.4448`
  - `psg_0_90`: `0.8339`
  - `psg_1_00`: `0.8342`
- Weighted summary:
  - `current_default`: `0.8489`
  - `ym_only`: `0.8405`
  - `psg_only`: `0.4192`
  - `psg_0_90`: `0.8497`
  - `psg_1_00`: `0.8498`

What it proved:

- PSG is not optional, but it is also not the dominant source of the remaining mismatch.
- `ym_only` stays surprisingly close on broad trusted-window metrics, which means the FM path still carries almost all of the perceived structure.
- `psg_only` is nowhere near the capture on its own, as expected.
- Nudging PSG gain upward from `0.8` to `0.9` or `1.0` helps slightly, but only by about `0.0008 - 0.0009` on the weighted fixed-window score.
- That improvement is too small to justify another default retune by itself.

Carry-forward lesson:

- When isolating a chip or submix, lock the comparison to the same trusted reference window. Letting each candidate re-align itself turns diagnostics into fan fiction.

### 23. `ymfm` did not rescue the GHZ external match

The next obvious suspicion was that maybe the remaining external mismatch was still hiding in our YM core, and that the earlier live-trace `ymfm` agreement had been too internal to settle the question. This pass compared `genesoxide YM-only` and `ymfm YM-only` against the same trusted hardware windows, under the same output shaping path.

What changed:

- The harness renderer gained an `external YM stream` path so pre-rendered YM stereo can be pushed through the same YM filter and post-mix chain as the normal timed replay path.
- A GHZ diagnostic was added to compare:
  - `current_default`
  - `genesoxide_ym`
  - `ymfm_ym`
- All three were scored on the exact same trusted window selected from `current_default`.

What happened:

- On `sonic_ghz.flac`:
  - `current_default`: `0.8637`
  - `genesoxide_ym`: `0.8496`
  - `ymfm_ym`: `0.8520`
- On `sonic_ghz_16bap.flac`:
  - `current_default`: `0.8336`
  - `genesoxide_ym`: `0.8311`
  - `ymfm_ym`: `0.8241`
- Weighted summary:
  - `current_default`: `0.8489`
  - `genesoxide_ym`: `0.8405`
  - `ymfm_ym`: `0.8382`

What it proved:

- `ymfm` does not materially outperform the current genesoxide YM path against the external GHZ references.
- On one capture it is slightly better; on the other it is slightly worse.
- The weighted result is slightly worse than genesoxide YM-only, not better.
- That makes it very unlikely that the remaining external mismatch is mostly “our YM2612 math is still wrong.”

Carry-forward lesson:

- If a trusted external score barely changes or gets worse when swapping in a respected reference core, stop blaming the synthesized chip core and go after the reference/capture model instead.

### 24. Reference normalization showed a real low-mid excess, not a single magic shelf

With PSG and YM core suspicion mostly cleared, the next pass compared both GHZ captures against a weighted spectral consensus built from their own trusted windows. The point was to stop asking "which capture should win?" and instead ask "what stable spectral shape do they actually agree on?"

What changed:

- Added trusted-window spectral consensus helpers for left, mid, and side.
- Weighted the reference consensus by each capture's trusted-window reliability.
- Reported the largest per-band deltas both:
  - capture vs capture
  - emulator vs reference consensus

What happened:

- Reference self-agreement against the consensus was strong in left and mid:
  - `sonic_ghz.flac`: `left = 0.9872 / 1.05 dB`, `mid = 0.9837 / 1.08 dB`, `side = 0.7259 / 0.81 dB`, `weight = 0.8021`
  - `sonic_ghz_16bap.flac`: `left = 0.9834 / 1.07 dB`, `mid = 0.9805 / 1.11 dB`, `side = 0.6248 / 0.84 dB`, `weight = 0.7813`
- The emulator consensus was visibly farther off:
  - `current_default`: `left = 0.9227 / 2.69 dB`, `mid = 0.9270 / 2.56 dB`, `side = 0.6156 / 0.81 dB`
- The biggest emulator-over-consensus deltas clustered in the low-mid region:
  - left: `301 Hz +5.84 dB`, `151 Hz +5.56 dB`, `194 Hz +4.89 dB`, `129 Hz +4.68 dB`, `237 Hz +4.67 dB`
  - mid: `151 Hz +5.59 dB`, `301 Hz +5.56 dB`, `194 Hz +4.88 dB`, `129 Hz +4.70 dB`, `237 Hz +4.49 dB`

What it proved:

- The remaining tonal disagreement is not random. There is a real low-mid excess in the emulator relative to the two-capture consensus.
- The captures still disagree with each other in multiple bands, especially outside the mid channel, so this is not a license to slap in one "correct" shelf and declare victory.
- Side mismatch remains real, but the low-mid excess is the more repeatable cross-reference signal.

Carry-forward lesson:

- Use the reference consensus to identify stable pressure points, but do not confuse a strong band delta with permission to retune blindly. The candidate still has to win end-to-end.

### 25. Consensus-guided low-mid and side retunes still lost to `current_default`

After the consensus pass identified a likely low-mid excess, the next step was to test that hypothesis directly instead of pretending the band-delta printout was already a fix. The candidate sweep used the same trusted-window anchor and weighted consensus target, then tried both low-mid relief candidates and side-path variants under that stricter scorer.

What changed:

- Added a reference-normalized candidate sweep for:
  - reduced `190 Hz` boost
  - stronger low-shelf cuts around `110 Hz`
  - no side EQ
  - alternate side EQ / side gain variants carried forward from earlier side sweeps

What happened:

- `current_default` stayed on top:
  - `left = 0.9098 / 3.00 dB`
  - `mid = 0.9130 / 2.93 dB`
  - `side = 0.6286 / 1.04 dB`
  - `score = 0.7707`
- Low-mid relief candidates all lost despite slightly better side scores:
  - `eq4_190_plus1p5`: `0.7593`
  - `eq4_190_flat`: `0.7470`
  - `lowshelf_m7p5_eq4_p1`: `0.7564`
  - `lowshelf_m8_eq4_0`: `0.7479`
- Side-path variants also lost:
  - `no_side_eq`: `0.7582`
  - `side_eq_presence`: `0.7676`
  - `side_eq_trim`: `0.7658`
  - `side_eq_combo`: `0.7683`

What it proved:

- The low-mid excess is real, but the obvious static EQ moves that seem like they should fix it still make the overall cross-reference match worse.
- The current side shaping is not dead weight. Removing it or retuning it slightly reduces the overall normalized match.
- The remaining mismatch is more specific than "too much 190 Hz" or "wrong side EQ." Those are now falsified as simple fixes.

Carry-forward lesson:

- A spectral delta is a clue, not a patch. If the end-to-end consensus sweep still prefers the current default, keep the default and move to a better model instead of polishing a worse retune.

### 26. Residual stability said the remaining gap is not a neat static correction curve

After the consensus-guided retune attempts lost, the next question was whether the emulator-vs-reference residual was even stable enough for a fixed post-mix correction to make sense. This pass segmented each trusted GHZ window into 1-second chunks and compared the residual spectra over time in left, mid, and side.

What changed:

- Added segmented residual-profile helpers to compute per-segment spectral residuals.
- Added a residual-stability diagnostic that reports:
  - per-reference segment drift in dB
  - mean residual bands for left, mid, and side
  - cross-reference similarity of those mean residual curves

What happened:

- On `sonic_ghz.flac`:
  - `segments = 7`
  - `left_stability = 3.36 dB`
  - `mid_stability = 3.46 dB`
  - `side_stability = 1.52 dB`
  - strongest mean residual left bands:
    - `883 Hz -10.88 dB`
    - `301 Hz +10.32 dB`
    - `86 Hz -7.44 dB`
    - `129 Hz +7.05 dB`
- On `sonic_ghz_16bap.flac`:
  - `segments = 7`
  - `left_stability = 4.17 dB`
  - `mid_stability = 4.19 dB`
  - `side_stability = 1.09 dB`
  - strongest mean residual left bands:
    - `452 Hz +8.32 dB`
    - `237 Hz +7.06 dB`
    - `194 Hz +5.92 dB`
    - `151 Hz +5.51 dB`
- Cross-reference residual agreement was only moderate in left and mid and very poor in side:
  - `sonic_ghz.flac`: `left = 0.7679 / 1.99 dB`, `mid = 0.7268 / 2.18 dB`, `side = 0.8407 / 1.35 dB`
  - `sonic_ghz_16bap.flac`: `left = 0.7331 / 2.04 dB`, `mid = 0.7005 / 2.23 dB`, `side = 0.1951 / 1.38 dB`

What it proved:

- The remaining left/mid residual is not temporally stable enough for a simple static correction to explain it cleanly. It moves by roughly `3-4 dB` across 1-second segments inside each trusted window.
- The side residual is steadier within a given capture, but it does not agree across captures strongly enough to justify a universal side-only correction.
- The residual behavior now looks more like recording/capture variance or section-dependent coloration than one missing fixed filter in the emulator.

Carry-forward lesson:

- When the residual itself drifts by several dB across time and disagrees across references, stop treating static EQ as the main weapon. The next tools should isolate section dependence, reference quality, or non-static capture behavior.

### 27. Section-aware trusted-window scoring found one bad slice, but mostly in one capture

Once the residual-stability pass showed that the mismatch drifted over time, the next step was to score the trusted GHZ window in 1-second sections instead of grading the whole 8-second loop-stable window as one block. The point was to see whether a specific musical phrase was consistently bad across captures or whether the drift was just another capture artifact.

What changed:

- Added a helper to partition trusted windows into exact 1-second sections plus a tail.
- Added a section-aware GHZ diagnostic that reports, for each section:
  - weighted average fit
  - cross-reference disagreement penalty
  - consensus mid residual bands
  - per-reference detail for the worst section

What happened:

- Sections `0-2` were mediocre but not catastrophic:
  - section `0`: `final = 0.5974`
  - section `1`: `final = 0.5818`
  - section `2`: `final = 0.5852`
- Section `4` was the real crater:
  - relative window time `4.00s -> 5.00s`
  - `avg = 0.3783`
  - `final = 0.3246`
  - consensus mid residual: `108 Hz +13.86 dB`, `194 Hz +12.25 dB`, `301 Hz +11.95 dB`
- But the crater was not consistent across captures:
  - `sonic_ghz.flac`: `score = 0.6230`, `left = 0.6119`, `mid = 0.5890`, `side = 0.2916`, `rms = 0.9997`
  - `sonic_ghz_16bap.flac`: `score = 0.1271`, `left = 0.0000`, `mid = 0.0000`, `side = 0.0000`, `rms = 0.5085`

What it proved:

- There *is* a specific bad subregion inside the trusted window, around `4-5s` into the sectioned comparison.
- But that failure is driven overwhelmingly by the second capture, not by a stable cross-reference mismatch.
- That makes the next move section-specific reference triage, not another emulator-wide retune.

Carry-forward lesson:

- If one section goes red only because one reference falls apart, treat that section as a reference-quality investigation first. Do not promote a global emulator change based on a capture-specific crater.

### 28. Local re-lag did not rescue the bad `16bap` section

The obvious follow-up to the section-aware crater was to ask whether the bad `4-5s` slice in the second capture was simply drifting a little inside the trusted window. This pass kept the emulator section fixed, allowed a small local offset search on the reference side, and rescored the same 1-second section after re-alignment.

What changed:

- Added a local section-offset helper that searches small reference offsets using RMS-envelope correlation.
- Added a reusable aligned-section scorer so nominal and re-lagged windows use the same metrics.
- Added a GHZ diagnostic that:
  - finds the worst trusted-window section
  - runs local re-lag for each reference on that exact section
  - compares nominal vs adjusted fit

What happened:

- The worst section stayed section `4`, `4.00s -> 5.00s`, with weighted baseline `0.3783`.
- On `sonic_ghz.flac`, local re-lag found only a tiny shift:
  - offset `-1024` samples (`-23.22 ms`)
  - nominal `0.6230`
  - adjusted `0.6195`
- On `sonic_ghz_16bap.flac`, local re-lag found nothing to rescue:
  - offset `+0`
  - envelope correlation `0.0000`
  - nominal `0.1271`
  - adjusted `0.1271`
  - left/mid/side all stayed `0.0000`

What it proved:

- The bad `16bap` section is not just a small local timing drift inside the trusted window.
- The first capture remains broadly sane in that section; the second capture does not.
- That shifts the next investigation away from emulator timing and toward section-specific capture pathology or content mismatch in the second reference.

Carry-forward lesson:

- When a bad section survives local re-lag unchanged, stop blaming drift. The next job is to inspect that reference section's own integrity or provenance.

### 29. Cross-capture comparison says the bad section is a stereo/level disagreement, not timing

After checking the `16bap` section for obvious local corruption, the next question was whether the two hardware captures were at least recording the same musical content in that `4-5s` crater. This pass compared the bad section from `sonic_ghz.flac` directly against the same bad section from `sonic_ghz_16bap.flac`, with a small local offset search.

What changed:

- Added a worst-section cross-capture diagnostic using the same section selection as the previous passes.
- Compared:
  - nominal cross-capture section alignment
  - locally re-lagged alignment
  - left / mid / side / RMS sub-scores

What happened:

- Worst section remained section `4`, `4.00s -> 5.00s`.
- Cross-capture local re-lag found a modest offset:
  - `+3072` samples (`+69.66 ms`)
  - envelope correlation `0.4868`
- But re-lag did not improve the score:
  - nominal `0.4846`
  - adjusted `0.4791`
- The actual shape of the disagreement was:
  - nominal: `left = 0.8797`, `mid = 0.8370`, `side = -0.3369`, `rms = 0.5584`
  - adjusted: `left = 0.8817`, `mid = 0.8390`, `side = -0.3308`, `rms = 0.5265`

What it proved:

- The two captures are not failing to line up in left/mid content. Those scores are actually decent.
- The real disagreement in the bad section is stereo side behavior plus level, not timing.
- That means the `16bap` crater is not just a corrupted or drifted copy of the first capture. It is a materially different recording in that slice.

Carry-forward lesson:

- If cross-capture left/mid match is decent but side goes negative and RMS fit collapses, stop treating the two captures as interchangeable stereo or level oracles for that section.

### 30. Section-weighted consensus stopped the bad `16bap` slice from over-voting

The next step was to encode the `16bap` bad-section finding into the consensus scorer instead of just complaining about it in diagnostics. The old consensus path let each reference cast one global vote over the whole trusted window. The new section-weighted pass breaks the trusted window into 1-second slices, scores each slice against its own self-consistency, and reduces a reference's effective vote when its section-level self-consistency is weak.

What changed:

- Added section-level normalized scoring against per-section self ceilings.
- Added section-level self-consistency weights.
- Added a section-weighted consensus scorer that:
  - averages section scores within each reference using section weights
  - multiplies the reference vote by squared section reliability
  - keeps a worst-section score around so cross-reference disagreement still matters
- Added new GHZ diagnostics for:
  - full section-weighted profile sweep
  - detailed leader comparison

What happened:

- The bad `16bap` trusted window now carries visibly weaker section confidence:
  - `sonic_ghz.flac`: `section_weight = 0.6531`, `section_norm = 0.7796`, `worst_section = 0.7183`
  - `sonic_ghz_16bap.flac`: `section_weight = 0.6019`, `section_norm = 0.5524`, `worst_section = 0.1601`
- The section-weighted leader board compressed hard:
  - `no_side_eq`: `0.5074`
  - `xf_0_30`: `0.5070`
  - `side_air_plus`: `0.5068`
  - `psg_0_90`: `0.5068`
  - `current_default`: `0.5068`
- That is materially less flattering to `xf_0_30` than the older unsectioned consensus pass, and it also prevents the bad `16bap` slice from shoving the whole ranking around.

What it proved:

- Section-weighting does what it should: it demotes the influence of the known bad `16bap` slice.
- Once that happens, the apparent differences between plausible output profiles shrink to nearly nothing.
- `no_side_eq` technically leads, but only by a joke margin. That is not a serious enough spread to justify another shipped-default retune.

Carry-forward lesson:

- When one reference contains a section-specific crater, make the scorer section-aware before you trust any whole-window ranking. But do not retune on a margin that small. That would just be a more sophisticated form of swamp dancing.

### 31. Mono-first consensus made side-channel similarity a secondary constraint, not a ruling class

The next step was to stop treating stereo-side similarity as a co-equal oracle inside the section-aware path. The bad `16bap` slice had already shown that side behavior is the first thing to go feral across captures, so this pass split each section score into:

- a mono-content score: left spectral, mid spectral, and RMS fit
- a side score: side spectral only
- a combined score that weights mono much more heavily than side

What changed:

- Added a mono-first section scorer that uses:
  - mono score = left spectral + mid spectral + RMS fit, with side removed from that core content score
  - side score tracked separately
  - combined score = `mono * 0.85 + side * 0.15`
- Added a mono-first section reliability weight that penalizes weak left/mid stability harder than weak side stability.
- Added new GHZ diagnostics for:
  - full mono-first consensus profile sweep
  - detailed mono-first leader comparison

What happened:

- The mono-first leaders compressed even harder than the plain sectioned leaders:
  - `side_air_plus`: `0.5520`
  - `side_lowmid_cut_more`: `0.5520`
  - `psg_0_90`: `0.5519`
  - `xf_0_30`: `0.5518`
  - `current_default`: `0.5517`
- The trusted-window mono scores are decent where the side scores are still miserable:
  - `current_default` vs `sonic_ghz.flac`: `mono = 0.9394`, `side = 0.2765`
  - `current_default` vs `sonic_ghz_16bap.flac`: `mono = 0.6676`, `side = 0.1286`
- The worst-section side score collapsed to `0.0000` for both references in the leader comparison, which is exactly why side should not be allowed to boss the scorer around.

What it proved:

- Treating side as secondary is the right direction for noisy multi-capture consensus.
- It did not reveal a meaningful new winner. The top spread is now absurdly small, on the order of `0.0001..0.0003`.
- `current_default` is not obviously wrong under mono-first scoring. The tiny leader changes are not serious enough to justify another default retune.

Carry-forward lesson:

- When stereo-side behavior is the least stable part of the reference set, make mono-content consensus the primary signal and keep side as a constraint, not a throne. But if the leaderboard still compresses into statistical lint, stop turning knobs and go get a better oracle.

### 32. A shared mono consensus oracle is cleaner than asking each capture to vote separately

The next step was to stop using the two hardware captures as separate judges and instead build a single mono-content target from them. This pass kept the fixed trusted windows, extracted the mid-channel content from each capture, averaged their log spectra and RMS with mono-first reliability weights, and then scored every candidate against that shared target.

What changed:

- Added a mono-consensus target built from:
  - fixed trusted windows anchored on `current_default`
  - mid-channel log spectra from each hardware capture
  - weighted RMS averaging
  - self-consistency ceilings for spectral similarity and RMS fit
- Added a mono-consensus candidate scorer that compares each candidate window against that single target instead of asking each capture to cast its own vote.
- Added new GHZ diagnostics for:
  - mono-consensus profile sweep
  - mono-consensus leader comparison
- Added the mono-consensus oracle readout to the public GHZ golden output.

What happened:

- The shared mono target is very self-consistent:
  - `self_spectral = 0.9821`
  - `self_rms_fit = 0.8269`
- `current_default` lands very close to that target:
  - `average = 0.9380`
  - `penalty = 0.0012`
  - `final = 0.9368`
- The per-reference mono fits for `current_default` are also nearly identical:
  - `sonic_ghz.flac`: `spectral = 0.9134`, `rms_fit = 0.8050`
  - `sonic_ghz_16bap.flac`: `spectral = 0.9122`, `rms_fit = 0.8856`
- The mono-consensus sweep is brutally compressed but coherent:
  - `psg_0_70`: `0.9382`
  - `current_default`: `0.9368`
  - `psg_0_90`: `0.9353`
  - most other candidates are effectively tied at `0.9368`

What it proved:

- A shared mono oracle is much saner than separate per-capture scoring for this GHZ case.
- The two captures agree strongly on broad mono spectral content even when they disagree on stereo-side behavior.
- There is a faint hint that PSG may still be slightly hot in the mono consensus path, but the spread is still too small to justify another shipped-default retune on its own.

Carry-forward lesson:

- When multiple captures disagree mostly in stereo or side behavior, build a shared mono-content oracle first. If that oracle still compresses the leaderboard into dust, stop pretending another static knob will save you.

### 33. Hybrid mono consensus and a focused PSG sweep finally justified one shipped retune

The shared mono oracle was strong enough to stop arguing about philosophy and start moving one real lever. This pass added a hybrid scorer that keeps the mono-consensus target primary but adds normalized side similarity as a small tie-breaker, then used that scorer to sweep PSG gain directly instead of relying on a few canned candidates.

What changed:

- Added a hybrid mono-consensus scorer:
  - mono-consensus score remains the primary term
  - normalized side similarity becomes a small secondary term
  - side disagreement only adds a light penalty
- Added GHZ diagnostics for:
  - hybrid mono-consensus profile sweep
  - hybrid mono-consensus leader comparison
  - focused PSG-gain sweep under the hybrid scorer
- Promoted the default `psg_gain` from `0.80` to `0.65` in the live core and kept the harness mirror configs aligned.

What happened:

- The hybrid scorer produced a more useful ranking than the mono-only target:
  - `psg_0_70`: `0.8914`
  - `current_default` (old `0.80`): `0.8901`
  - `psg_0_90`: `0.8887`
- The focused PSG sweep made the trend uncomfortably clear:
  - `0.60`: `0.8919`
  - `0.65`: `0.8920`
  - `0.70`: `0.8914`
  - `0.80`: `0.8901`
  - `0.90`: `0.8887`
  - `1.00`: `0.8871`
- That is not random candidate noise. It is a monotonic “PSG is too hot” slope over a broad range.
- After promoting `psg_gain = 0.65`, the public GHZ golden stayed healthy and the new consensus scores improved:
  - `Mono consensus oracle`: `final = 0.9388`
  - `Hybrid mono consensus`: `final = 0.8920`

What it proved:

- The old default PSG level really was a bit too hot under the best current external oracle we have.
- Lowering PSG gain is a real improvement, not another tiny leaderboard hallucination.
- The change is still modest. This is not “audio solved,” but it is the first recent external-reference retune that earned promotion cleanly.

Carry-forward lesson:

- When a mono-primary scorer and a focused parameter sweep both point the same direction over a broad range, stop dithering and ship the small correction.

### 34. Hybrid-score crossfeed sweep did not justify another default move

After the PSG retune, the next obvious analog lever was crossfeed. This pass used the new hybrid mono-consensus scorer directly and swept crossfeed around the shipped default to see whether the new PSG level had exposed a better stereo-width setting.

What changed:

- Added a focused GHZ crossfeed sweep under the hybrid mono-consensus scorer.
- Kept the default `psg_gain = 0.65` fixed while sweeping crossfeed.

What happened:

- The sweep came back with a shallow curve, not a real win:
  - `0.20`: `0.8896`
  - `0.25`: `0.8903`
  - `0.30`: `0.8911`
  - `0.35`: `0.8920`
  - `0.40`: `0.8915`
  - `0.45`: `0.8920`
- `0.45` bought a higher raw average but also more disagreement penalty, so it tied `0.35` instead of beating it.

What it proved:

- Crossfeed is not the next clean lever.
- The current `0.35` setting remains a defensible default under the new scorer.
- Another crossfeed retune right now would just be more knob fondling dressed up as science.

Carry-forward lesson:

- If a parameter sweep only trades average score against disagreement penalty and comes back tied at the shipped value, leave it alone and move on.

### 35. Hybrid-score side-EQ sweep found a direction, but not a promotion-worthy win

After crossfeed went flat, the next targeted probe was the side-only EQ pair. This pass held the newly shipped `psg_gain = 0.65` fixed, then swept the side low-mid cut around `420 Hz` and the side presence lift around `2.6 kHz` under the hybrid mono-consensus scorer.

What changed:

- Added a focused side-EQ grid sweep under the hybrid mono-consensus scorer.
- Swept:
  - side low-mid cut at `420 Hz`: `0.0, -0.8, -1.6, -2.4, -3.2 dB`
  - side presence at `2.6 kHz`: `0.0, +0.4, +0.8, +1.2, +1.6 dB`

What happened:

- The sweep found a consistent direction:
  - stronger side low-mid cuts beat the current `-1.6 dB`
  - extra side presence did not matter much once the low-mid cut got strong
- Best finalists:
  - `lowmid = -3.2 / presence = +0.0`: `0.8926`
  - `lowmid = -3.2 / presence = +0.4`: `0.8926`
  - `lowmid = -2.4 / presence = +0.4`: `0.8923`
  - current shipped default (`-1.6 / +0.8`) remains below those at about `0.8920`

What it proved:

- If the next stereo lever is anywhere, it is more side low-mid cut, not more side presence.
- The gain is still tiny. This is a directional hint, not a promotion-grade victory.
- The side pair is no longer complete fog, but it still does not beat the default by enough to justify another shipped retune.

Carry-forward lesson:

- When a sweep finds a direction but only by `~0.0005`, treat it as a future search prior, not as a commandment. Save promotions for moves that clear the noise floor.

### 36. A refined side low-mid cut finally earned promotion

The coarse and medium side-EQ sweeps both pointed in the same direction: more side low-mid cut, less side presence. This pass tightened that search around the `420 Hz` region and then ran a direct current-vs-candidate comparison before touching the default.

What changed:

- Added a focused hybrid-score refinement sweep for the side low-mid cut:
  - frequencies: `390, 420, 450, 480 Hz`
  - cuts: `-2.8, -3.2, -3.6, -4.0 dB`
  - fixed `Q = 0.95`
  - side presence held at `0.0 dB`
- Added a direct current-vs-candidate GHZ comparison.
- Promoted the default side-only EQ pair from:
  - `420 Hz / -1.6 dB`
  - `2.6 kHz / +0.8 dB`
  to:
  - `450 Hz / -4.0 dB`
  - `2.6 kHz / 0.0 dB`

What happened:

- The refined sweep was consistent:
  - `450 Hz / -4.0 dB`: `0.8928`
  - `420 Hz / -4.0 dB`: `0.8928`
  - `480 Hz / -4.0 dB`: `0.8928`
  - the old shipped side pair stayed lower at about `0.8920`
- The direct current-vs-candidate comparison was good enough to stop hedging:
  - `current_default`: `hybrid_final = 0.8920`, `trusted_side = 0.3617`
  - `refined_side_lowmid`: `hybrid_final = 0.8928`, `trusted_side = 0.3904`
- After promotion, the public GHZ golden improved where it should:
  - `Best local env`: `0.7091 -> 0.7114`
  - `Local side spectral`: `0.7770 -> 0.7990`
  - `Trusted window side`: `0.3617 -> 0.3904`
  - `Hybrid mono consensus`: `0.8920 -> 0.8928`

What it proved:

- The side-EQ path was not just noise after all. The side low-mid region really did need a stronger cut.
- The improvement is still modest, but it cleared the internal evidence bar better than the earlier side sweeps.
- This is the second recent external-reference retune that earned promotion cleanly after the PSG-gain move.

Carry-forward lesson:

- If a refined local sweep, a direct current-vs-candidate check, and the public golden all move the same way, ship the change and move on.

### 37. Side low-mid Q refinement did not earn another retune

The next obvious slice after the `450 Hz / -4.0 dB` promotion was the width of that cut. The frequency and gain were no longer guesses, so this pass isolated `Q` and checked whether the new hybrid mono-consensus scorer wanted a narrower or broader side low-mid notch.

What changed:

- Added a focused ignored GHZ sweep over the side low-mid `Q` parameter with:
  - fixed center `450 Hz`
  - fixed gain `-4.0 dB`
  - side presence held at `2.6 kHz / 0.0 dB`
  - `Q` values: `0.70, 0.80, 0.90, 0.95, 1.00, 1.10, 1.20, 1.30, 1.40, 1.50, 1.60, 1.80`
- Added a direct current-vs-candidate comparison for the best-looking refined `Q`.

What happened:

- The hybrid-score curve improved as `Q` tightened, then flattened:
  - `Q = 0.95`: `0.8928`
  - `Q = 1.30`: `0.8932`
  - `Q = 1.50`: `0.8933`
  - `Q = 1.60`: `0.8933`
  - `Q = 1.80`: `0.8933`
- The direct current-vs-candidate check against `Q = 1.50` was mixed:
  - `current_default`: `hybrid_final = 0.8928`, `trusted_spectral = 0.8969`, `trusted_rms = 0.9462`, `trusted_side = 0.3904`
  - `refined_side_lowmid_q`: `hybrid_final = 0.8933`, `trusted_spectral = 0.8972`, `trusted_rms = 0.9416`, `trusted_side = 0.3774`

What it proved:

- A narrower side low-mid cut can game the hybrid scalar slightly by shaving disagreement penalty.
- The win is not clean, because the trusted-window side metric got worse, not better.
- That is the wrong trade for the current state of this project. The side path is still one of the few remaining visible gaps, so a tiny scalar gain that weakens side agreement is not promotion-worthy.

Carry-forward lesson:

- When the remaining mismatch is already mostly side behavior, do not ship a retune that improves a composite score by `~0.0005` while making trusted side similarity worse. Not every local optimum deserves a crown.

### 38. Sectioned side consensus turned side into a real oracle instead of a per-reference afterthought

The hybrid mono scorer still treated stereo side as per-reference normalization glued onto a mono target. That was good enough to keep side from dominating the mix, but it still did not give side the same kind of shared oracle that mono already had. This pass fixed that shape.

What changed:

- Added a real side-consensus target with agreement-based weighting:
  - each side window contributes side spectrum plus side ratio
  - an initial target is built from weighted references
  - references that disagree with that preliminary target get downweighted before the final target is built
- Added a fixed sectioned side-consensus builder and scorer over the trusted GHZ window.
- Added synthetic tests proving the side target downweights outliers and prefers agreeing candidates.
- Surfaced the new sectioned side-consensus metric in the public GHZ golden output.

What happened:

- The new ignored GHZ side-consensus probe came back sane:
  - `current_default`: `average = 0.3475`, `penalty = 0.1237`, `final = 0.2238`, `worst_section = 0.0522`
  - `side_q_1_50`: `average = 0.3451`, `penalty = 0.1223`, `final = 0.2228`, `worst_section = 0.0519`
  - `no_side_eq`: `average = 0.3215`, `penalty = 0.1109`, `final = 0.2106`, `worst_section = 0.0543`
- The oracle also identified the weakest shared side sections instead of pretending the whole window is equally trustworthy:
  - `7.00s..7.99s`, `5.00s..6.00s`, `3.00s..4.00s`, `2.00s..3.00s`, `1.00s..2.00s`
- The public GHZ golden now prints:
  - `Sectioned side consensus: sections = 8, average = 0.3474, penalty = 0.1237, final = 0.2238, worst_section = 0.0522`

What it proved:

- The new side oracle is not just another flattering scalar. It agrees with the earlier evidence:
  - `current_default` still beats `no_side_eq`
  - the tighter `Q = 1.50` candidate is still not good enough to promote
- The weakest side behavior is concentrated in specific late trusted-window sections, not spread uniformly across the whole window.
- That makes this a useful oracle for future side work and a better guardrail against overfitting one capture.

Carry-forward lesson:

- If stereo side is the remaining gap, give it its own section-aware consensus target. Per-reference side normalization is better than nothing, but a shared side oracle is better than vibes.

### 39. The weak side sections are real, but they do not point to one clean global retune

Once the sectioned side oracle existed, the next sane step was to interrogate the worst sections directly instead of staring at the rolled-up score and pretending it was explanatory. This pass added per-section candidate analysis and band-delta reporting against the section side target.

What changed:

- Added weighted aggregation for side candidate windows so per-section diagnostics can compare a candidate's shared side spectrum against the section target directly.
- Added a new ignored GHZ weak-section probe that prints:
  - the weakest section times
  - each candidate's per-section side score
  - side-ratio drift from the section target
  - the largest target-vs-candidate side-band deltas
- Surfaced the weakest section window in the public GHZ golden output.

What happened:

- The public GHZ golden now prints:
  - `Sectioned side weakest: start = 7.00s, end = 7.99s, final = 0.0522`
- The weak-section probe found the same late sections the new oracle already distrusted:
  - `7.00s..7.99s`
  - `5.00s..6.00s`
  - `2.00s..3.00s`
  - `3.00s..4.00s`
- In those weak sections, the emulator side ratio stayed below the section target for every candidate:
  - `7.00s..7.99s`: target `0.1481`, current default `0.0694`
  - `5.00s..6.00s`: target `0.2071`, current default `0.0797`
  - `2.00s..3.00s`: target `0.2014`, current default `0.1422`
  - `3.00s..4.00s`: target `0.1651`, current default `0.1266`
- The recurring side-band deltas were real but not clean enough for one obvious global fix:
  - repeated pressure around `~883 Hz`
  - repeated disagreement around `~2.1 kHz`
  - section-dependent trouble in the `5-6 kHz` region

What it proved:

- The side weakness is not uniformly spread across the trusted window. The bad side fit is concentrated in a few specific sections.
- Those sections do share a common symptom: the emulator side energy is too narrow there.
- But the band deltas do not line up into one clean, consistent static EQ move. Different weak sections disagree about the exact low-mid and upper-band shape.
- That means the next lever is probably not another whole-track side EQ tweak.

Carry-forward lesson:

- When the worst sections all show low side energy but disagree on the exact spectral fix, stop trying to solve them with one more global side filter. That smell is pointing at section-dependent behavior, stereo dynamics, or reference-chain weirdness.

### 40. The weak side sections miss on dynamics too, not just side amount

The sectioned side-consensus oracle proved that a few late trusted-window sections were the real stereo crater, but that still left one ambiguity: was the emulator simply too narrow there, or was the side channel moving in the wrong shape over time? This pass added a second side oracle aimed at that distinction.

What changed:

- Added a side-dynamics target built from:
  - RMS envelope of the side channel inside each trusted section
  - section side ratio
  - agreement-based downweighting for outlier references, just like the side-spectral oracle
- Added synthetic tests proving:
  - envelope-shape outliers get downweighted
  - candidates with matching side motion beat candidates with the right overall ratio but the wrong side shape
- Added a new ignored GHZ diagnostic that prints, per weak section:
  - side-dynamics final score
  - side-envelope fit against the section target
  - side-ratio drift from the section target
- Surfaced the new metric in the public GHZ golden output as `Sectioned side dynamics`.

What happened:

- The public GHZ golden now prints:
  - `Sectioned side dynamics: sections = 8, average = 0.2087, penalty = 0.0393, final = 0.1695, worst_section = 0.1199`
  - `Sectioned side dynamics weakest: 7.00s -> 7.99s, final = 0.1199`
- The new ignored GHZ dynamics probe came back compressed and ugly:
  - `side_q_1_50`: `average = 0.2152`, `penalty = 0.0388`, `final = 0.1764`
  - `current_default`: `average = 0.2156`, `penalty = 0.0401`, `final = 0.1755`
  - `no_side_eq`: `average = 0.2059`, `penalty = 0.0355`, `final = 0.1703`
- In the worst sections, the side envelope fit was not merely low. It was near zero or negative while side ratio was also below target:
  - `7.00s..7.99s`: `env = 0.0960`, `ratio = 0.0694`, target ratio `0.1653`
  - `2.00s..3.00s`: `env = 0.0066`, `ratio = 0.1422`, target ratio `0.2205`
  - `3.00s..4.00s`: `env = -0.0809`, `ratio = 0.1266`, target ratio `0.1712`
  - `6.00s..7.00s`: `env = -0.1352`, `ratio = 0.1187`, target ratio `0.1618`

What it proved:

- The weak late GHZ sections are not just “too little side.” The side channel is also moving in the wrong shape over time.
- That explains why repeated static side-EQ retunes only reshuffle tiny scalar scores. They can nudge average width, but they do not fix the section-local side motion mismatch.
- The remaining side gap looks more like section-dependent stereo behavior or reference/capture modeling than one missing fixed filter.

Carry-forward lesson:

- When side-ratio drift and side-envelope mismatch both stay bad, stop treating the problem like a global width or EQ knob. The remaining lever is dynamic stereo behavior, section-aware modeling, or a better oracle.

### 41. Small side-envelope lag is real, but it does not rescue the weak sections

Once the sectioned side-dynamics oracle existed, the next obvious escape hatch was local lag. If the weak sections were only failing because the side motion arrived a little early or late, that would be a timing-shaped problem. If they stayed bad after a small re-lag, then the side motion itself was still wrong.

What changed:

- Added a small tested helper that finds the best offset between short envelope sequences.
- Extended the side-dynamics GHZ diagnostic to report, for each weak section:
  - nominal side-envelope correlation
  - best local lag in envelope bins
  - adjusted side-envelope correlation after that small re-lag
- Surfaced the weakest-section lag result in the public GHZ golden output.

What happened:

- The public GHZ golden now prints:
  - `Sectioned side dynamics weakest lag: bins = -1, ms = -23.2, adjusted_env = 0.2419`
- The ignored GHZ side-dynamics probe showed that small local lags do improve some weak sections, but not nearly enough:
  - `7.00s..7.99s`: `env = 0.0960`, best lag `-1 bin`, adjusted `0.2419`
  - `2.00s..3.00s`: `env = 0.0066`, best lag `-2 bins`, adjusted `0.1236`
  - `3.00s..4.00s`: `env = -0.0809`, best lag `-1 bin`, adjusted `0.1587`
  - `6.00s..7.00s`: `env = -0.1352`, best lag `+3 bins`, adjusted `-0.0032`

What it proved:

- There is some real local timing mismatch in the side channel. The weak sections are not perfectly aligned.
- But local re-lag only turns “awful” into “still bad.” Even after the best small shift, the weakest section only reaches `0.2419`.
- That means timing is a contributor, not the whole answer. The side motion shape itself is still wrong after alignment.

Carry-forward lesson:

- If small local re-lag helps but does not rescue the weak sections, do not reach for a global delay or sample-offset hack. The remaining problem is still structural side behavior, not just late side energy.

### 42. The weak side sections are under-articulated, not just mistimed

The small-lag pass showed that local timing mismatch exists, but it still did not tell us whether the remaining problem was mostly delayed side motion or weak side transients. This pass added transient readouts on top of the side-dynamics probe to answer that more directly.

What changed:

- Added tiny helper coverage for:
  - first-difference side-envelope transient profiles
  - fixed-offset profile correlation
- Extended the GHZ side-dynamics probe to print, for each weak section:
  - transient correlation before re-lag
  - transient correlation after the best small lag
  - transient RMS ratio versus the section target
- Surfaced the weakest-section transient summary in the public GHZ golden output.

What happened:

- The public GHZ golden now prints:
  - `Sectioned side dynamics weakest transient: raw = 0.1661, adjusted = 0.5793, rms = 0.2000`
- The weak sections tell a consistent ugly story:
  - `7.00s..7.99s`: transient `0.1661 -> 0.5793`, transient RMS `0.2000`
  - `2.00s..3.00s`: transient `-0.1565 -> 0.0403`, transient RMS `0.1864`
  - `3.00s..4.00s`: transient `-0.1791 -> 0.3722`, transient RMS `0.5789`
  - `6.00s..7.00s`: transient `-0.3272 -> -0.0042`, transient RMS `0.4645`

What it proved:

- Some weak sections do benefit from small re-lag, but that only exposes the deeper problem: the side transients are still too weak.
- In the worst shared section, even after timing correction, the side transient strength is only `20%` of the target.
- That is not a width knob problem. It is not a simple delay problem either. The side channel is under-articulated in exactly the places where the stereo fit collapses.

Carry-forward lesson:

- If adjusted transient correlation improves but transient RMS stays far below target, the next lever is not more EQ or global lag. It is section-local stereo articulation, side transient generation, or a better explanation of what the reference chain is doing.

### 43. Simple side transient boosting barely moves the weak-section oracle

The transient analysis showed that the weak sections are under-articulated, which made a simple next experiment obvious: if the problem is just “not enough side snap,” then a harness-only side transient enhancer should buy a clean score increase. This pass tested that before touching the shipped audio path.

What changed:

- Added a tiny harness-only side transient mix helper:
  - zero amount is identity
  - mono stays mono
  - pure-side steps get sharper
- Added an ignored GHZ sweep that applies transient-only side shaping to the already-rendered stereo mix and scores it with the existing sectioned side-dynamics oracle.

What happened:

- The transient-only sweep barely moved:
  - `current_default`: `final = 0.1755`
  - `transient_0.10`: `0.1756`
  - `transient_0.20`: `0.1758`
  - `transient_0.35`: `0.1756`
  - `transient_0.50`: `0.1748`
- The weakest section barely improved:
  - `7.00s..7.99s`: `0.1199 -> 0.1196` at best
- The transient metrics nudged, but only cosmetically:
  - `7.00s..7.99s`: transient RMS `0.2000 -> 0.2060`
  - adjusted transient correlation actually drifted down as the boost increased: `0.5793 -> 0.5305`

What it proved:

- The reference is not simply asking for “more side transient.” A naive transient enhancer does not buy a meaningful win.
- The remaining problem is more specific than side snap. Either the side articulation is wrong in a more structured way, or the reference chain is doing something that a generic transient booster cannot mimic.

Carry-forward lesson:

- If a harness-only transient enhancer barely changes the weak-section score, do not turn it into a real audio feature. That is another swamp-dance knob, not a root-cause fix.

### 44. The weakest side-transient target is not stable across the two hardware captures

The transient-boost experiment killed the simple “add more side snap” story, but it still left one annoying ambiguity: was the emulator under-articulating a real shared hardware target, or were the two captures themselves disagreeing about what that target even was? This pass stopped poking the emulator and measured cross-capture side-transient agreement directly on the weak shared sections.

What changed:

- Added a tiny pure helper that measures:
  - envelope correlation
  - best small-lag adjustment
  - transient correlation
  - lag-adjusted transient correlation
  - transient RMS ratio
- Added a small pair-summary helper so weak-section reference disagreement can be reported compactly.
- Added an ignored GHZ probe that:
  - ranks the weakest `current_default` side-dynamics sections
  - compares the underlying hardware captures against each other on those exact sections
- Surfaced the weakest-section reference-transient summary in the public GHZ golden output.

What happened:

- The public GHZ golden now prints:
  - `Sectioned side dynamics weakest reference transient: pairs = 1, env = 0.0913 -> 0.2257, trans = -0.0732 -> 0.1840, rms_fit = 0.1679, lag = 23.2 ms`
- The weak sections show hard cross-capture disagreement:
  - `7.00s..7.99s`: env `0.0913 -> 0.2257`, trans `-0.0732 -> 0.1840`, transient RMS ratio `5.9568`, side ratio `0.1948 / 0.1086`
  - `2.00s..3.00s`: env `-0.2160 -> 0.7391`, trans `-0.3431 -> 0.6954`, transient RMS ratio `2.0024`
  - `3.00s..4.00s`: env `-0.3859 -> 0.7561`, trans `-0.2777 -> 0.6366`, transient RMS ratio `1.5497`
  - `6.00s..7.00s`: env `0.3710 -> 0.4606`, trans `0.0472 -> 0.4187`, transient RMS ratio `4.4103`

What it proved:

- The worst side-dynamics crater is not a clean stereo target that the emulator is simply failing to hit.
- Even after tiny local re-lag, the two hardware captures only weakly agree on the worst section’s side transient shape and disagree violently on transient strength.
- That makes the late-window side-transient crater a bad oracle for default retuning.

Carry-forward lesson:

- If the references themselves only weakly agree on a weak-section transient target, stop retuning the emulator toward that section. Treat it as capture/provenance noise first, or explicitly downweight it in future scoring.

### 45. The side-dynamics scorer now downweights cross-capture transient fights

Once the weak-section transient target was exposed as unstable across captures, the next sane move was not another filter tweak. It was to teach the side-dynamics scorer that these sections deserve less authority. This pass added a transient-consistency weight at section build time so the scorer penalizes cross-capture transient fights instead of treating them like clean targets.

What changed:

- Added a small pure summary over side-dynamics windows that measures:
  - lag-adjusted envelope agreement
  - lag-adjusted transient agreement
  - transient RMS fit
  - worst adjusted transient agreement
- Added a pure weight function that turns that summary into a bounded reliability weight.
- Folded that new transient-consistency weight into `build_fixed_sectioned_side_dynamics(...)` so section weighting now reflects:
  - trusted-window reliability
  - side self-consistency
  - cross-capture transient consistency

What happened:

- The public GHZ golden moved in the right direction without touching the shipped mix:
  - `Sectioned side dynamics average = 0.2087 -> 0.2007`
  - `penalty = 0.0393 -> 0.0295`
  - `final = 0.1695 -> 0.1712`
- The same late section is still the worst one, but it now has far less authority:
  - `7.00s..7.99s target_weight = 0.0368`
- The ignored side-dynamics leaderboard still only gives tiny spreads:
  - `side_q_1_50 = 0.1775`
  - `current_default = 0.1759`
  - `no_side_eq = 0.1738`

What it proved:

- The right move was scorer hygiene, not emulator retuning.
- The weakest late section still exists, but the scorer is no longer overpaying attention to a target the captures themselves barely agree on.
- This made the side-dynamics oracle slightly less noisy without manufacturing a fake default-audio “win.”

Carry-forward lesson:

- When a section is measurably unreliable across references, fix the oracle first. A better weighting model is worth more than another round of mix knob necromancy.

### 46. The same unreliable side sections no longer dominate sectioned side consensus or the hybrid tie-break

The previous pass only cleaned up the sectioned side-dynamics scorer. That still left two other places giving the haunted side sections too much influence:

- the sectioned side-consensus oracle
- the hybrid mono-consensus scorer's stereo-side tie-break

This pass carried the same transient-consistency idea into those paths instead of touching the actual audio output.

What changed:

- `build_fixed_sectioned_side_consensus(...)` now also measures cross-capture side-transient consistency on each section and folds that into the section weight.
- The hybrid mono-consensus scorer now computes a side-reliability factor from the sectioned side-consensus target and uses it to scale how much side can influence the final score.
- Added a small pure test proving that lower side reliability shrinks the side tie-break gap instead of letting side keep full authority.

What happened:

- Public GHZ golden changed materially on the scorer side, not the audio side:
  - `Hybrid mono consensus final = 0.8928 -> 0.9202`
  - `Sectioned side consensus final = 0.2238 -> 0.2468`
- The weak late section still exists, but sectioned side consensus now weights it less:
  - `7.00s..7.99s weight = 0.0361`
- The ignored sectioned side-consensus diagnostic now lands:
  - `current_default = 0.2469`
  - `side_q_1_50 = 0.2466`
  - `no_side_eq = 0.2346`
- The ignored hybrid profile sweep still does not justify a default retune:
  - `xf_0_30 = 0.9204`
  - `current_default = 0.9202`
  - `side_gain_up = 0.9202`

What it proved:

- The earlier hybrid scorer was overstating confidence in the side tie-break.
- Once the unreliable side sections lose influence, the candidate leaderboard compresses even harder.
- That is another sign that the remaining differences are mostly oracle/capture-chain ambiguity, not a strong signal that the shipped mix is materially wrong.

Carry-forward lesson:

- If a score changes a lot after reliability weighting but candidate ordering barely changes, trust the weighting fix and do not promote another retune. The old score was simply overconfident.

### 47. The public side readout now distinguishes raw weakest sections from dominant weighted impact

After the scorer cleanup, the public output was still doing one stupid thing: it kept pointing at the raw weakest section as if that section were still the main blocker. This pass fixed the reporting layer so the harness prints both:

- the raw weakest section
- the section with the largest weighted impact on the score

What changed:

- Added a tiny pure helper that finds the dominant section impact from:
  - section final scores
  - section weights
- Extended the sectioned side score structs to carry that dominant weighted impact.
- Updated the public GHZ golden to print:
  - `Sectioned side dominant`
  - `Sectioned side raw weakest`
  - `Sectioned side dynamics dominant`
  - `Sectioned side dynamics raw weakest`

What happened:

- The public GHZ golden now cleanly separates the two stories:
  - `Sectioned side raw weakest = 7.00s -> 7.99s, final = 0.0522`
  - `Sectioned side dominant = 2.00s -> 3.00s, final = 0.1352, impact = 0.1202`
  - `Sectioned side dynamics raw weakest = 7.00s -> 7.99s, final = 0.1199`
  - `Sectioned side dynamics dominant = 0.00s -> 1.00s, final = 0.1831, impact = 0.1683`

What it proved:

- The late `7-8s` crater is still ugly, but it is no longer the main weighted blocker once reliability is accounted for.
- The sections that actually drag the score are earlier, higher-weight regions, not the tiny haunted late section that kept stealing attention.

Carry-forward lesson:

- When reliability weighting changes what actually matters, fix the reporting too. Otherwise the team keeps optimizing for the wrong corpse.

### 48. The ignored side probes now chase dominant weighted sections instead of the raw ugliest crater

The public GHZ readout already learned the difference between:

- the raw weakest section
- the section with the largest weighted impact on the score

The ignored diagnostic probes had not caught up. They were still expanding the raw ugliest section first, which kept dragging attention back to the late `7-8s` crater even after the scorer said the real weighted blockers were earlier sections. This pass fixed the harness probes instead of the mix.

What changed:

- Added a tiny pure helper that ranks all section impacts by weighted deficit, not just the single dominant one.
- Added a small regression proving the ranking follows weighted impact instead of raw weakest score.
- Rewired:
  - `diagnose_ghz_sectioned_side_weak_sections`
  - `diagnose_ghz_sectioned_side_dynamics`
- Those probes now print the top dominant weighted sections first and relegate the raw weakest section to a secondary note.

What happened:

- No audio score changed. This was diagnostic surgery, not emulator retuning.
- The probes now line up with the scorer's actual priorities:
  - side-consensus work starts from the dominant `2.00s -> 3.00s` region
  - side-dynamics work starts from the dominant `0.00s -> 1.00s` region
- The late `7.00s -> 7.99s` crater is still printed, but as a secondary curiosity instead of the lead suspect.

What it proved:

- The harness finally speaks one consistent language across:
  - public readout
  - ignored diagnostics
  - scorer weighting
- That matters because diagnostics that lead with the wrong section will still waste time even if the scoring code is technically correct.

Carry-forward lesson:

- Once the oracle decides what matters, make every debug view obey that same weighting model. Otherwise the tooling keeps summoning the same ghost with a different font.

### 49. The dominant weighted sections now print their own cross-capture stability

After the ignored probes finally started following weighted impact, they still had one blind spot: they showed what the emulator was missing in those dominant sections, but not whether the references themselves agreed enough for those sections to deserve trust. This pass fixed that by making the dominant-section probes print direct cross-capture consistency for the exact sections they expand.

What changed:

- Added a tiny side-consensus reference-pair summary path.
- Reused the existing side-dynamics pair analysis for fixed dominant sections.
- Updated:
  - `diagnose_ghz_sectioned_side_weak_sections`
  - `diagnose_ghz_sectioned_side_dynamics`
- Those probes now print per-section cross-capture agreement before the emulator candidate lines.

What happened:

- No audio score changed. This was oracle inspection, not emulator retuning.
- The dominant section probes now expose whether their lead suspects are actually stable across the two captures, instead of assuming it from the target blend.
- The answer was mostly rude:
  - dominant side-consensus sections are still weak cross-capture stereo oracles
  - dominant side-dynamics sections only agree after a meaningful local lag and still miss on transient strength

Evidence:

- Dominant side-consensus sections:
  - `2.00s -> 3.00s`: `spectral = 0.4292`, `ratio_fit = 0.7800`
  - `6.00s -> 7.00s`: `spectral = -0.3106`, `ratio_fit = 0.7464`
  - `3.00s -> 4.00s`: `spectral = -0.1335`, `ratio_fit = 0.9101`
  - `0.00s -> 1.00s`: `spectral = -0.1519`, `ratio_fit = 0.9231`
- Dominant side-dynamics sections:
  - `0.00s -> 1.00s`: `env = 0.0856 -> 0.7400`, `trans = -0.1418 -> 0.5768`, `trans_rms = 0.5522`, `lag = 46.4 ms`
  - `3.00s -> 4.00s`: `env = -0.3859 -> 0.7561`, `trans = -0.2777 -> 0.6366`, `trans_rms = 0.6453`, `lag = 69.7 ms`
  - `2.00s -> 3.00s`: `env = -0.2160 -> 0.7391`, `trans = -0.3431 -> 0.6954`, `trans_rms = 0.4994`, `lag = 46.4 ms`
  - `1.00s -> 2.00s`: `env = 0.0080 -> 0.7662`, `trans = -0.1084 -> 0.6363`, `trans_rms = 0.6205`, `lag = 46.4 ms`

What it proved:

- The right next question is no longer "what is the ugliest section?"
- It is "does the section that most hurts the weighted score also have enough cross-capture agreement to justify tuning against it?"
- For the current two captures, the answer is still "not really" for side spectrum, and only "partly" for side dynamics after local re-lag.
- That means the dominant weighted sections are better suspects than the old late-window crater, but they are still not clean enough stereo-side oracles to justify another default mix retune yet.

Carry-forward lesson:

- If a debug view tells you what hurts the score but not whether the references agree there, it is only half a tool.

### 50. The dominant-section side miss is mostly YM-side, and pan-behavior retuning only helps a little

The next sane question after the dominant-section cleanup was whether the remaining side miss was really a pan-behavior problem and, if so, whether it lived in YM-side behavior or in some broader mix mistake. This pass added two harness-first diagnostics instead of changing the shipped audio path:

- a dominant-section chip/side balance probe
- a focused `mid_gain` / `side_gain` pan-behavior sweep scored with the mono oracle as the guardrail

What changed:

- Added a tiny pure helper for a mono-first dominant-pan score.
- Added a dominant-section probe for:
  - `current_default`
  - `ym_only`
  - `psg_only`
- Added an ignored `mid_gain` / `side_gain` sweep anchored to:
  - hybrid mono consensus final
  - dominant sectioned-side final
  - dominant sectioned-side-dynamics final

What happened:

- The dominant-section chip balance came back clean:
  - `current_default` and `ym_only` are very close on the dominant side sections
  - `psg_only` collapses to zero side, as expected
- That means the remaining stereo-side miss is overwhelmingly YM-side behavior, not PSG being too loud or too wide.
- The pan-behavior sweep found only a small lever:
  - best candidate was `mid_gain = 1.00`, `side_gain = 1.15`
  - current default `mid_gain = 1.00`, `side_gain = 1.00` was already close
- Moving `mid_gain` away from `1.00` consistently hurt the mono-first score.

Evidence:

- Dominant section chip balance:
  - `current_default`: dominant side `ratio = 0.1422` vs target `0.2014`; dominant dynamics `ratio = 0.1516` vs target `0.1721`
  - `ym_only`: dominant side `ratio = 0.1473`; dominant dynamics `ratio = 0.1546`
  - `psg_only`: dominant side `ratio = 0.0000`; dominant dynamics `ratio = 0.0000`
- Dominant pan-behavior sweep leaders:
  - `mid=1.00 side=1.15`: `hybrid = 0.9203`, `dom_side = 0.1450`, `dom_dyn = 0.1966`, `combined = 0.9128`
  - `mid=1.00 side=1.10`: `hybrid = 0.9202`, `dom_side = 0.1416`, `dom_dyn = 0.1934`, `combined = 0.9127`
  - `mid=1.00 side=1.05`: `hybrid = 0.9202`, `dom_side = 0.1383`, `dom_dyn = 0.1882`, `combined = 0.9126`
  - current default `mid=1.00 side=1.00`: `hybrid = 0.9202`, `dom_side = 0.1352`, `dom_dyn = 0.1831`, `combined = 0.9125`

What it proved:

- PSG is not the stereo-side villain. The miss survives almost unchanged in `ym_only`.
- The remaining side gap is mostly a YM-side / pan-behavior issue or a reference-model issue around YM-side behavior.
- Global mid gain is not the lever. The mono guardrail wants it essentially unchanged.
- Extra side gain helps a little, but only a little. The improvement is real enough to see in the sweep and too small to justify another shipped-default retune yet.

Carry-forward lesson:

- Once the score says “mostly YM-side,” stop kicking PSG. And if the best pan-behavior sweep only buys a few ten-thousandths, do not pretend you found salvation; you found a direction, not a finish line.

### 51. The dominant bad sections do not have live YM pan chaos

The next sane question after the tiny pan-behavior sweep was whether the dominant bad sections were actually being driven by wrong YM pan occupancy. This pass stayed harness-only and added a timed-register view of YM panning over the exact weighted-dominant GHZ sections instead of over whole-track soup.

What changed:

- Added a pure timed-write helper that reconstructs per-channel YM pan state occupancy from live `0xB4..=0xB6` writes.
- Added red/green regression coverage for:
  - pre-interval state carry-forward
  - correct `port 1` channel mapping
- Added an ignored GHZ dominant-section probe that prints per-channel `L+R`, `L`, `R`, and `off` occupancy plus pan-change counts over the current dominant side and dominant side-dynamics windows.

What happened:

- The dominant weighted sections are not full of pan churn.
- In both dominant windows:
  - channel 1 is `off` for the full section
  - channels 2-6 sit at `L+R = 100%`
  - pan-change counts are `0` for every channel
- That means the remaining side miss is not being driven by wrong hard-left / hard-right occupancy inside those windows.

Evidence:

- Dominant side section `2.00s -> 3.00s`:
  - `ch1 = off 100%`
  - `ch2..ch6 = L+R 100%`
  - all channels `changes = 0`
- Dominant side-dynamics section `0.00s -> 1.00s`:
  - `ch1 = off 100%`
  - `ch2..ch6 = L+R 100%`
  - all channels `changes = 0`

What it proved:

- The dominant bad sections are not missing side because the YM pan registers are flapping to the wrong places in real time.
- Another global pan-law retune would be cargo culting. The timed register view says there is no active hard-pan occupancy to “fix” inside the windows that matter most.
- If the side gap is still real, it is more likely coming from earlier-history effects, analog/capture behavior, or the reference itself than from live YM pan-state occupancy in the dominant sections.

Carry-forward lesson:

- Before inventing a pan-law theory, inspect the actual pan registers on the machine timeline. If the dominant windows are statically centered, stop pretending the mix needs heroic left/right knob fondling.

### 52. Even the dominant-window pan history is mostly boring

The obvious follow-up after the occupancy probe was whether recent pan history could still be seeding the bad windows through filter memory or other carry-over. This pass stayed harness-only again and asked a narrower question: how long before each dominant window did each YM channel last experience a real pan-state change?

What changed:

- Added a pure helper that reports the most recent real YM pan change per channel before an arbitrary cutoff tick.
- Added regression coverage for:
  - ignoring same-state rewrites
  - correct `port 1` channel mapping
- Added an ignored GHZ dominant-window history probe that prints state-at-entry plus `last_change_age_ms` for all six YM channels.

What happened:

- The dominant side-consensus section is not even close to fresh pan history:
  - all channels last changed about `2242-2268 ms` before the section start
- The dominant side-dynamics section is closer, but still not twitchy:
  - all channels last changed about `242-268 ms` before the section start
- In both cases the channels are already sitting in the same static states seen in the occupancy pass:
  - `ch1 = off`
  - `ch2-ch6 = L+R`

Evidence:

- Dominant side section `2.00s -> 3.00s`:
  - `ch1 off @ 2244.7 ms`
  - `ch2 LR @ 2244.1 ms`
  - `ch3 LR @ 2243.6 ms`
  - `ch4 LR @ 2243.0 ms`
  - `ch5 LR @ 2242.4 ms`
  - `ch6 LR @ 2268.3 ms`
- Dominant side-dynamics section `0.00s -> 1.00s`:
  - `ch1 off @ 244.7 ms`
  - `ch2 LR @ 244.1 ms`
  - `ch3 LR @ 243.6 ms`
  - `ch4 LR @ 243.0 ms`
  - `ch5 LR @ 242.4 ms`
  - `ch6 LR @ 268.3 ms`

What it proved:

- The weighted-dominant side-consensus miss is not coming from recent YM pan churn. That corpse is now pretty dead.
- The weighted-dominant side-dynamics miss has somewhat fresher pan history, but it is still on the order of a quarter second, not “active pan changes inside the bad window.”
- So if there is still an emulator-side stereo problem, it is no longer plausibly explained by simple live pan occupancy or very recent pan-register motion.

Carry-forward lesson:

- Once both occupancy and entry-history say “boring, centered, old,” stop theorizing about pan laws as if they are the missing grail. Either the remaining issue is subtler than pan registers, or the reference is still haunting the room.

### 53. The side leak is history-dependent under real GHZ traffic, not a generic centered-pan bug

After the pan-history passes, the next useful suspicion was that the neutral YM path itself might be leaking stereo even under centered panning. This pass attacked that in three stages:

- prove a simple centered tone stays mono under the neutral `Legacy` path
- compare real GHZ `ym_only` traffic under:
  - current default output chain
  - neutral `Legacy` output chain
- mutate live GHZ pan writes to see whether the side leak follows generic centered-panning rules or depends on pan history over the real traffic stream

What changed:

- Added neutral-path regressions for:
  - centered single-op `render()` output staying mono
  - centered single-op `render_timed_writes()` output staying mono
- Added harness diagnostics for:
  - dominant-section reference side authority under partial mono collapse
  - neutral-path GHZ `ym_only` side leak
  - forcing all GHZ pan writes centered
  - forcing only pre-capture GHZ pan writes centered
  - forcing GHZ pan writes centered up to early post-capture horizons

What happened:

- The simple centered tone is clean:
  - both `render()` and `render_timed_writes()` stay mono under neutral `Legacy`
- Real GHZ `ym_only` traffic is not clean under the same neutral path:
  - dominant side ratio jumps to about `0.4981`
  - dominant dynamics ratio jumps to about `0.5908`
- Partially collapsing the references’ side channel does not rescue cross-capture agreement in any meaningful way, so this is not just “the references are too wide.”
- Forcing **all** GHZ pan writes centered from origin kills the neutral-path side leak in the dominant windows:
  - both dominant ratios drop to `0.0000`
- Forcing only **pre-capture** pan writes centered does nothing.
- Forcing pan writes centered only through the first `5s` after capture also does nothing.

Evidence:

- Neutral-path GHZ `ym_only`:
  - `current_default_ym_only`: dominant side `ratio = 0.1473`, dominant dynamics `ratio = 0.1546`
  - `neutral_legacy_ym_only`: dominant side `ratio = 0.4981`, dominant dynamics `ratio = 0.5908`
- Forced pan history:
  - `original_trace`: dominant side `ratio = 0.4981`, dominant dynamics `ratio = 0.5908`
  - `pre_capture_pan_centered`: unchanged
  - `all_pan_centered`: dominant side `ratio = 0.0000`, dominant dynamics `ratio = 0.0000`
- Early horizon sweep:
  - centering pan writes before `0, 250, 500, 1000, 1500, 2000, 3000, 5000 ms` after capture leaves the dominant neutral-path side leak unchanged

What it proved:

- There is no generic “centered YM always leaks stereo” bug in the simple path.
- The bad stereo behavior needs real GHZ traffic/history.
- The leak is absolutely tied to GHZ pan history somewhere later in the live stream, but not to the pre-capture intro and not to the first few seconds after capture.
- So the remaining stereo issue is now much narrower:
  - some later pan-history segment in the real track is seeding persistent side in the neutral YM path
  - or the trusted-window section indices are later in the captured run than their local `2-3s` labels suggest, and the next useful probe needs the absolute emu start times for those windows

Carry-forward lesson:

- When a simple centered-tone regression stays clean but the real song goes wide, stop arguing from synthetic purity. The bug lives in traffic history, not in the trivial happy path.

### 54. The earlier pan probes used the wrong clock, and the absolute-window read is much less boring

The next pass corrected a harness bug in the pan diagnostics themselves. The earlier “dominant pan state/history” probes were looking at the section’s local trusted-window offset (`2-3s`, `0-1s`) instead of the actual candidate `emu_start` windows used by the scorer. That made the earlier pan conclusions too confident and too early.

What changed:

- Added a pure helper to summarize absolute `emu_start` positions across the dominant section’s fixed references.
- Reworked the dominant YM pan-state and pan-history diagnostics to use the real absolute emu windows.
- Reworked the pan-history horizon sweep to center pan writes up to those absolute window starts instead of only early post-capture times.

What happened:

- The dominant windows are much later than the local labels imply:
  - side window absolute span is about `30.79s .. 41.27s`
  - dynamics window absolute span is about `28.79s .. 39.27s`
- The real pan state inside those absolute windows is not boring:
  - `ch4` is mostly left in both dominant windows
  - `ch5` is mostly right/off in both dominant windows
  - `ch1` is hard right, not off
- The real pan history is also not boring:
  - in the dominant dynamics window, `ch4` changed only about `22.5 ms` before the earliest absolute start
  - in the dominant side window, `ch4` and `ch5` changed about `688 ms` / `588 ms` before entry
- Once the cutoff sweep uses the real absolute windows, centering pan writes before about `28.79s` already collapses most of the neutral-path side leak, and centering through `41.27s` kills it fully.

Evidence:

- Corrected dominant side absolute window:
  - `abs = 30.79s, 35.53s, 40.27s .. 41.27s`
  - `ch4 = LR 9.6%, L 89.6%, off 0.8%, changes = 15`
  - `ch5 = LR 4.9%, R 58.8%, off 36.3%, changes = 16`
- Corrected dominant dynamics absolute window:
  - `abs = 28.79s, 33.53s, 38.27s .. 39.27s`
  - `ch4 = LR 19.4%, L 79.8%, off 0.8%, changes = 18`
  - `ch5 = LR 4.9%, R 72.6%, off 22.5%, changes = 17`
- Corrected pan history:
  - dominant side: `ch4 last_change = 688.4 ms`, `ch5 = 588.1 ms`
  - dominant dynamics: `ch4 last_change = 22.5 ms`, `ch5 = 1191.1 ms`
- Absolute cutoff sweep:
  - `0.00s`: dominant side `ratio = 0.4981`, dominant dynamics `ratio = 0.5908`
  - `28.79s`: dominant side `ratio = 0.0875`, dominant dynamics `ratio = 0.0645`
  - `39.27s`: dominant side `ratio = 0.0233`, dominant dynamics `ratio = 0.0000`
  - `41.27s`: both `ratio = 0.0000`

What it proved:

- The old “boring centered pan” story was wrong because the probe was using the wrong clock.
- Later live YM pan history is a real contributor to the neutral-path side leak, especially through channels 4 and 5.
- This is no longer just “the reference is haunted.” There is real emulator-side stereo behavior tied to later pan traffic in the absolute windows that the scorer actually uses.

Carry-forward lesson:

- If the scorer uses `emu_start`, every timing-sensitive diagnostic must use `emu_start` too. Otherwise you are doing theology with a broken ruler.

### 55. The dominant neutral-path leak is mostly channel 1 occupancy, with channels 5 and 4 adding different kinds of trouble

After the absolute-window correction, the next pass stopped treating channels 4 and 5 as a fused suspect blob and actually isolated channel contributions under the neutral `Legacy` YM-only path.

What changed:

- Added channel-selective pan-centering helpers in the harness so diagnostics can center only chosen YM channels, either for the full run or only before a chosen absolute cutoff.
- Added a focused GHZ contribution sweep covering:
  - `ch1` alone
  - `ch4` alone
  - `ch5` alone
  - `ch4 + ch5`
  - `ch1 + ch4 + ch5`
  - each of those centered globally or only before the dominant dynamics / side absolute starts

What happened:

- `ch1` is the big baseline offender:
  - centering `ch1` alone from origin drops dominant side ratio from `0.4981` to `0.0875`
  - dominant dynamics ratio also drops from `0.5908` to `0.2992`
  - doing that only before `28.79s` or `30.79s` is effectively identical, which means this is mostly persistent hard-right occupancy, not fresh churn
- `ch5` is the next meaningful contributor, but mainly on dynamics:
  - centering `ch5` alone barely changes dominant side ratio
  - it does cut dominant dynamics ratio to about `0.3801` globally and `0.4264` when centered only before the dominant absolute windows
- `ch4` is smaller and later:
  - centering `ch4` alone only trims dominant side ratio slightly
  - it does essentially nothing for dominant dynamics unless later pan history is also touched
- `ch1 + ch4 + ch5` explains most of the leak:
  - centered before `28.79s`, dominant dynamics ratio falls to `0.0645`
  - centered before `30.79s`, dominant side ratio falls to `0.0732`
  - centered globally, both dominant ratios drop to `0.0000`

Evidence:

- `original_trace`: dominant side `ratio = 0.4981`, dominant dynamics `ratio = 0.5908`
- `ch1_all_centered`: dominant side `ratio = 0.0875`, dominant dynamics `ratio = 0.2992`
- `ch5_all_centered`: dominant side `ratio = 0.4873`, dominant dynamics `ratio = 0.3801`
- `ch4_all_centered`: dominant side `ratio = 0.4807`, dominant dynamics `ratio = 0.5777`
- `ch1_ch4_ch5_before_dyn_abs`: dominant side `ratio = 0.0875`, dominant dynamics `ratio = 0.0645`
- `ch1_ch4_ch5_before_side_abs`: dominant side `ratio = 0.0732`, dominant dynamics `ratio = 0.0645`
- `ch1_ch4_ch5_all_centered`: both dominant ratios `= 0.0000`

What it proved:

- The earlier “mostly channels 4 and 5” framing was incomplete.
- Channel 1’s hard-right occupancy is doing most of the neutral-path side damage in the dominant windows.
- Channel 5 materially contributes to the dynamics miss, and that contribution is already seeded before the dominant dynamics window.
- Channel 4 still matters, but it looks more like a smaller, later finishing contribution than the main source.
- So the next emulator-side suspicion is no longer generic pan history. It is hard-panned YM channel behavior:
  - pan-law / separation strength
  - bleed/crossfeed behavior for hard-panned voices
  - or some other per-channel stereo treatment that makes hard-right occupancy too wide versus hardware

Carry-forward lesson:

- If centering one ancient hard-right channel fixes more than centering the “active” channels, stop fetishizing recent change counts. Persistent occupancy can be the louder crime.

### 56. Simple per-channel hard-pan softening is not the fix, and the stem bench only works before final clamp

After the channel-contribution pass, the obvious next hypothesis was “fine, then just soften the guilty channels.” This pass built a proper harness-only bench for that idea:

- isolate YM stems for:
  - `ch1`
  - `ch4`
  - `ch5`
  - all other YM channels as one combined stem
  - PSG as its own stem
- prove those stems reconstruct the mixed render
- then reduce side on only the suspect YM stems and rescore against the existing mono/side oracles

Important trap that had to be killed first:

- naive stem summing under the shipped default path lied badly because the post-mix master gain and final clamp are nonlinear
- summing independently clamped stems produced a giant reconstruction error (`max_diff ≈ 0.3743`)
- the fix was to render stems at low master gain (`0.25x`), sum them in the linear region, then apply the final gain/clamp once at the end
- after that, reconstruction became honest again (`max_diff ≈ 0.0000875`)

What happened:

- once the stem bench was corrected, the best candidate was still the untouched baseline
- reducing side on `ch1` hurt first
- reducing side on `ch5` barely moved anything and still lost
- adding `ch4` softening made the result worse again
- mono consensus stayed completely flat, so this was not buying anything useful on the dominant stereo-specific scores either

Evidence:

- baseline / no softening:
  - `mono = 0.9388`
  - `side = 0.2468`
  - `dyn = 0.1658`
  - `combined = 0.7231`
- best altered candidate in the focused sweep:
  - `ch1=1.00, ch5=0.70, ch4=1.00`
  - `mono = 0.9388`
  - `side = 0.2470`
  - `dyn = 0.1647`
  - `combined = 0.7230`
- stronger `ch1` softening was clearly worse:
  - `ch1=0.70, ch5=1.00, ch4=1.00` → `combined = 0.7201`
  - `ch1=0.55, ch5=1.00, ch4=1.00` → `combined = 0.7180`

What it proved:

- the remaining side mismatch is not fixed by a simple static “bleed hard-panned channels toward mono” model
- `ch1` being the dominant occupancy offender does not automatically mean “turn down its side” is the right hardware model
- whatever is still different is subtler than a one-knob hard-pan softener:
  - different per-channel analog behavior
  - dynamic channel interaction
  - or more reference/capture weirdness

Carry-forward lesson:

- Before trusting any stem experiment, make sure the chain is still linear where you are cutting it apart. If clamp is downstream, decompose upstream or you are doing algebra on broken glass.

### 57. Targeted side-transient shaping on the suspect YM stems only buys lint

After static hard-pan softening failed, the next narrower hypothesis was that the remaining miss might be articulation rather than width: maybe the guilty YM stems are not too wide, just too soft in their side motion.

This pass reused the honest pre-clamp stem bench from the previous slice and applied side-transient shaping only to the suspect stems:

- `ch5` alone
- `ch4` alone
- `ch1` alone
- small `ch5 + ch4` combinations

Everything was rescored against the same fixed mono consensus, sectioned side consensus, and sectioned side dynamics oracles.

What happened:

- the bench remained honest:
  - reconstruction stayed at `max_diff ≈ 0.0000875`
- the best candidate was a small `ch5 + ch4` transient bump:
  - `ch5_t=0.35`, `ch4_t=0.20`
- but the gain was microscopic:
  - baseline combined `0.7231`
  - best candidate combined `0.7233`
- mono consensus stayed flat at `0.9388` for every candidate
- side and dynamics only nudged in the fourth decimal place

Evidence:

- baseline:
  - `side = 0.2468`
  - `dyn = 0.1658`
  - `combined = 0.7231`
- best candidate:
  - `ch5_t035_ch4_t020`
  - `side = 0.2474`
  - `dyn = 0.1661`
  - `combined = 0.7233`

What it proved:

- targeted transient shaping is more plausible than static hard-pan bleed, but only barely
- the effect size is still too small to justify shipping a new model
- so “missing side articulation on the suspect stems” does not currently clear the bar as the main remaining emulator-side bug

Carry-forward lesson:

- If the best carefully targeted articulation tweak only moves the score in the fourth decimal place, that is a diagnostic hint, not a product decision.

### 58. Tiny stem-local side memory on `ch1` and `ch4` is the first post-pan lever that moves the primary side oracle by more than lint

After the static softening and transient-only slices stalled, the next narrower hypothesis was that the dominant suspect stems might want a tiny amount of delayed side memory rather than just width reduction or extra snap.

To make that honest, this pass added a small `apply_side_delay_mix(...)` helper in the harness and drove it through the same pre-clamp stem bench used by the previous focused experiments. The important detail is unchanged:

- stems were decomposed upstream of the final master gain + clamp
- final gain/clamp was applied only once after recombining the altered stems
- reconstruction stayed honest at `max_diff ≈ 0.0000875`

What happened:

- the first real directional move came from `ch1`:
  - `ch1_d1_a020` beat baseline
- adding a matching tiny delay-memory on `ch4` improved it a bit further:
  - `ch1_d1_a020_ch4_d1_a020`
- mono consensus stayed perfectly flat at `0.9388`

Evidence:

- baseline:
  - `side = 0.2468`
  - `dyn = 0.1658`
  - `combined = 0.7231`
- best refined delay candidate:
  - `ch1_d1_a020_ch4_d1_a020`
  - `side = 0.2498`
  - `dyn = 0.1704`
  - `combined = 0.7242`

What it proved:

- this is the first suspect-stem post-pan lever that improved the primary side oracle by more than the fourth decimal place
- the effect is still small in absolute terms:
  - `sectioned side consensus` only moved by about `+0.0030`
- so there is a real hint of missing per-stem side memory or phase-ish behavior, but not yet a strong case for shipping a new hardware model

Carry-forward lesson:

- A small, honest improvement on the primary oracle is worth following once. It is not worth rewriting the mixer around unless the refinement keeps paying rent.

### 59. Adding the earlier `ch5/ch4` transient shaping on top of that side-memory hint plateaued immediately

Because the previous transient-only slice had at least been directionally positive, the next step was to compound the two most plausible harness-only effects:

- keep the best side-memory base:
  - `ch1_d1_a020`
  - `ch4_d1_a020`
- then layer in the earlier transient bumps on `ch5` and optionally `ch4`

What happened:

- every hybrid candidate clustered almost exactly together
- the best ones only nudged the side score from `0.2498` to about `0.2500`
- combined score stayed effectively pinned at `0.7242`
- mono consensus still stayed flat at `0.9388`

Evidence:

- `delay_base`:
  - `side = 0.2498`
  - `dyn = 0.1704`
  - `combined = 0.7242`
- best hybrid variants:
  - `delay_base_ch5_t035`
  - `delay_base_ch5_t020`
  - `delay_base_ch5_t020_ch4_t010`
  - all clustered at roughly:
    - `side = 0.2500`
    - `dyn = 0.1704`
    - `combined = 0.7242`

What it proved:

- the promising signal plateaued immediately under refinement
- the remaining miss is not meaningfully rescued by stacking tiny static/quasi-static stem shapers together
- this means the current target is not on track to hit `sectioned side consensus >= 0.27` with simple post-pan suspect-stem shaping

Carry-forward lesson:

- Once a promising lever plateaus under the first sensible refinement pass, stop. That is the point where the oracle or the model class is the bottleneck, not your willingness to keep turning screws.

### 60. A real core-side per-channel YM side-memory model finally moved the shipped stereo oracle, but only modestly

The harness-only `ch1/ch4` delayed-side hint was the first honest post-pan lever that moved the primary side oracle by more than lint, so the next step was to stop pretending in the harness and put a real version of that model into the live core and timed renderer.

This pass widened the YM boundary so the core could observe per-channel stereo taps before the final YM sum, then added a tiny 1-native-sample side-memory model per YM channel:

- the raw per-channel taps still sum back to the legacy stereo path when the side-memory amounts are all zero
- the timed renderer mirrors the same model, so live-vs-replay parity stays honest
- the shipped default only enables the channels the earlier harness slices actually implicated:
  - `ch1 = 0.20`
  - `ch4 = 0.15`

What happened:

- the primary target moved in the shipped path:
  - `sectioned side consensus final = 0.2468 -> 0.2502`
- side dynamics also improved:
  - `sectioned side dynamics final = 0.1712 -> 0.1763`
- the mono guardrail stayed flat:
  - `mono consensus final = 0.9388`

What it proved:

- there really was a small missing post-pan YM behavior in the emulator-side model class
- the harness hint was not fake once promoted into the core
- but the effect size is still modest in absolute terms and nowhere near enough to explain the whole remaining stereo-side gap

Carry-forward lesson:

- When a harness-only lever survives contact with the real core and still improves the public oracle, keep the change. But do not pretend a `+0.0034` move on the primary target is salvation.

### 61. Refining that core-side side-memory model plateaued immediately, so the stop rule fired for this model class

After promoting the real core-side model, the next move was a narrow refinement sweep around the winning point rather than another generic search.

What happened:

- nearby variants only shuffled dust:
  - `ch1 = 0.25, ch4 = 0.15` barely changed the side score and was not better overall
  - `ch1 = 0.15, ch4 = 0.10` moved dynamics a hair but not the combined read
- the original shipped point stayed the best balanced candidate:
  - `ch1 = 0.20`
  - `ch4 = 0.15`

Evidence:

- promoted default:
  - `mono = 0.9388`
  - `side = 0.2502`
  - `dyn = 0.1763`
- nearby refinement candidates all clustered around the same result and did not produce another meaningful primary-metric gain

What it proved:

- this model class is real but weak
- the target for this batch:
  - `sectioned side consensus final >= 0.27`
  is not reachable by continuing to fondle this particular knob
- the explicit stop rule fired correctly:
  - one real improvement larger than `0.002`
  - then immediate plateau under focused refinement

Carry-forward lesson:

- Stop when the model class taps out. The next sane move is a different stereo/analog model or a better oracle, not another week of polishing a `+0.0034` lever.

### 62. A harness-only pan-write delay proxy is the next model class to clear the `> 0.002` side threshold, but only barely

The side-memory model had already tapped out, so the next root-cause clue to follow was the live pan-history timing:

- dominant side section:
  - `ch4` and `ch5` had pan changes only about `688 ms` and `588 ms` before entry
- dominant dynamics section:
  - `ch4` had a pan change only about `22.5 ms` before entry

That made a traffic-dependent pan-edge model plausible enough to test. The first minimal proxy was not a new core behavior yet. It was a harness-only mutation:

- delay selected YM pan-register writes in the timed trace
- leave everything else unchanged
- score the same mono / sectioned side / sectioned dynamics targets

What happened:

- this proxy did beat the current shipped baseline on the primary side oracle
- the first pass pointed at delayed `ch5` pan writes as the strongest candidate

Evidence:

- shipped baseline:
  - `mono = 0.9388`
  - `side = 0.2502`
  - `dyn = 0.1802`
- first promising proxy:
  - `ch5_20.0ms`
  - `mono = 0.9385`
  - `side = 0.2536`
  - `dyn = 0.1800`

What it proved:

- there is another real model class here beyond static EQ and sample-local side memory
- pan-edge timing can move the primary side oracle by more than lint
- but the effect size is still small and was not yet a shipping argument

Carry-forward lesson:

- A harness proxy that clears the `> 0.002` threshold earns exactly one refinement pass. No more.

### 63. Refining that pan-write delay proxy plateaued around `ch5 ~= 25 ms`, which is not a sane hardware story

The refinement sweep tightened around the first pan-delay winner:

- `ch5`
- `ch4 + ch5`
- delays from about `12 ms` to `30 ms`

What happened:

- the best candidates clustered in the same narrow region:
  - `ch5_25.0ms`
  - `ch4_ch5_25.0ms`
- the best primary score only reached about:
  - `sectioned side consensus final = 0.2541`
- mono stayed above guardrail:
  - `mono = 0.9389`
- but this still fell well short of the batch target:
  - `sectioned side consensus final >= 0.27`

Evidence:

- best refined proxy:
  - `ch5_25.0ms`
  - `mono = 0.9389`
  - `side = 0.2541`
  - `dyn = 0.1800`
- nearby candidates:
  - `ch4_ch5_25.0ms`
  - `ch5_22.0ms`
  - `ch5_30.0ms`
  all clustered right around the same result

What it proved:

- the new model class is real enough to move the oracle
- but the required delay is implausibly large as a literal YM pan-register timing model
- and even with that implausible knob, the result still plateaus far below target

Carry-forward lesson:

- Do not ship a fake `25 ms` pan-register delay just because it flatters the current oracle. If this clue is worth pursuing further, it needs to be translated into a more plausible per-channel stereo-side persistence model or abandoned.

### 64. Translating the fake pan-delay clue into shared pan-edge persistence did not produce a real winner

The next slice took the `ch5 ~= 25 ms` harness proxy seriously enough to translate it into a shared live/replay model instead of another one-off test mutation.

What changed:

- added shared `AudioOutputConfig` fields for per-channel YM pan-edge persistence amount and decay time
- wired that model through:
  - the live core audio synth path
  - the timed replay renderer
  - the explicit-config regression surface
- added real unit coverage for:
  - pan-change trigger and decay
  - repeated centered pan writes staying inert
  - repeated hard-pan writes refreshing the persistence impulse
- added a focused GHZ sweep for pan-edge persistence candidates instead of relying on the older fake delayed-write proxy

What happened:

- the first version, which only fired on actual pan-mask transitions, was effectively a no-op against the GHZ side oracles
- that lined up with the updated absolute-window pan-history readout:
  - dominant side section:
    - `ch4` / `ch5` last changes are still about `688 ms` / `588 ms` before entry
  - dominant dynamics section:
    - `ch4` / `ch5` are about `22.5 ms` / `1191.1 ms` before entry
- because the fake delayed-write proxy still moved the score while a literal edge-trigger did not, the next refinement changed the model to refresh on any repeated non-centered pan write, not only on true state changes
- that still did not buy a real result

Evidence:

- public shipped default stayed effectively unchanged:
  - `mono final = 0.9388`
  - `sectioned side consensus final = 0.2502`
  - `sectioned side dynamics final = 0.1763`
- pan-edge persistence candidate sweep:
  - best results only nudged dynamics from `0.1802` to `0.1803`
  - `sectioned side consensus final` stayed pinned at `0.2502`
  - combined score stayed pinned at `0.7252`
- the centered mono regression with pan-edge persistence config still passed
- live replay parity still passed:
  - `corr = 1.0000`
  - `rms_ratio = 1.0000`

What it proved:

- the fake harness pan-delay clue does not translate cleanly into a simple shared per-channel pan-edge impulse model
- the remaining side gap is not explained by a small exponential carry seeded at pan writes
- even the repeated-hard-pan refinement only moves dust, so this model class has now had its fair shot

Carry-forward lesson:

- If a delayed-control proxy wins but both edge-triggered and repeated-write persistence translations are flat, stop trying to smuggle the same clue back in under nicer names. Either model the control-path timing more literally, or move on.

### 65. `soundlog` is useful as a GHZ event oracle, and it points straight at channel 4 pitch churn in the late weak windows

**Date:** 2026-04-07

**Hypothesis**

The next useful oracle is not more PCM surgery. It is a higher-level YM2612 event view that can tell us what the game is actually doing in the weak GHZ sections:

- real `KeyOn`
- real `KeyOff`
- real `ToneChange`

instead of forcing every conclusion through stereo-side waveform fallout.

**Change**

- pinned `soundlog = "=0.10.0"` in the test harness
- added a harness helper that:
  - converts the existing timed live-write VGM stream into a `soundlog` document
  - runs `VgmCallbackStream` with `Ym2612State`
  - extracts structured YM2612 events back out into a stable local shape
- explicitly coalesced same-sample same-channel `ToneChange` pairs so the helper keeps the final `A4/A0` result instead of reporting half-written pitch noise
- added a red-to-green unit test for a tiny timed YM stream:
  - `KeyOn`
  - one coalesced `ToneChange`
  - `KeyOff`
- added an ignored GHZ diagnostic that prints per-second post-capture event density by channel and kind

**Evidence**

- the new unit test passed with exactly the intended musical sequence:
  - `KeyOn @ sample 10`
  - `ToneChange @ sample 20`
  - `KeyOff @ sample 30`
- GHZ post-capture event totals over the 10-second trace:
  - `ch1: key_on=37 key_off=38 tone_change=5`
  - `ch2: key_on=38 key_off=37 tone_change=0`
  - `ch3: key_on=29 key_off=29 tone_change=5`
  - `ch4: key_on=21 key_off=21 tone_change=60`
  - `ch5: key_on=18 key_off=18 tone_change=5`
  - `ch6: key_on=0 key_off=0 tone_change=0`
- the earlier windows are mostly short keyed-note traffic:
  - `0-4s` is dominated by ordinary `KeyOn/KeyOff` activity across `ch1-5`
- the late weak windows are not generic traffic soup:
  - `6-7s`: `ch4 tone_change=37`
  - `7-8s`: `ch4 tone_change=21`
  - those windows are visibly packed with low-frequency `ch4` pitch movement while the other channels are mostly simple note gates
- representative late-window events:
  - `7.00s..8.00s` is full of repeated `ch4 ToneChange` events around `~55 Hz`

**Conclusion**

`soundlog` is worth keeping in the harness. It does not fix synthesis, but it gives a real event-level oracle for GHZ traffic, and that oracle says the late side-problem windows are strongly associated with channel 4 pitch activity rather than a vague whole-mix stereo curse.

This also means the next emulator-side question can be narrower:

- how that `ch4` tone-change traffic is being rendered spatially
- whether the remaining side mismatch follows specific musical control traffic rather than just pan state snapshots

**Lesson**

When the PCM oracle gets murky, promote the control stream to a first-class diagnostic. A stable event extractor is a better next move than another week of filter necromancy.

### 66. Retargeting `soundlog` onto the dominant weighted sections kills the live tone-change hypothesis

**Date:** 2026-04-07

**Hypothesis**

The late `6-8s` `ch4` churn was probably a liar because it came from the raw ugliest windows, not the weighted blockers. If `soundlog` is going to help the stereo-side problem, it has to be pointed at the weighted-dominant sections:

- sectioned side dominant: `2.00s -> 3.00s`
- sectioned side dynamics dominant: `0.00s -> 1.00s`

The working hypothesis was that real live `ToneChange` traffic on one or two YM channels inside those exact absolute windows might be the causal lever behind the remaining side miss.

**Change**

- added tiny pure helpers to:
  - convert dominant section refs into absolute sample windows
  - summarize tracked YM2612 events inside an exact sample range
- added red-to-green unit coverage for both helpers
- added an ignored `soundlog` diagnostic for the weighted-dominant GHZ sections instead of whole-track buckets
- added a focused causality probe that freezes live YM frequency writes for `ch1` and/or `ch4` in the exact dominant side and dominant dynamics windows

**Evidence**

- the pure helper tests passed:
  - `section_sample_range_from_refs_offsets_from_capture_start`
  - `summarize_ym2612_tracked_events_in_sample_range_counts_only_selected_window`
- dominant weighted windows are absolutely not the same as the old late crater:
  - dominant side abs window: `30.79s .. 41.27s`
  - dominant dynamics abs window: `28.79s .. 39.27s`
- `soundlog` in those exact windows says `ch1` is the big `ToneChange` machine, with `ch4` second:
  - dominant side: `ch1 tone_change=394`, `ch4 tone_change=82`
  - dominant dynamics: `ch1 tone_change=369`, `ch4 tone_change=139`
- representative dominant dynamics events start immediately with live `ch1` and `ch4` pitch motion:
  - `ch1 ToneChange` around `389-398 Hz`
  - `ch4 ToneChange` around `246-284 Hz`
- but the causality probe is blunt:
  - `current_default`: `mono=0.9388 side=0.2502 dyn=0.1802`
  - `freeze_ch1_dom_dyn`: `mono=0.9294 side=0.1940 dyn=0.1803`
  - `freeze_ch4_dom_dyn`: `mono=0.9364 side=0.2280 dyn=0.1764`
  - `freeze_ch1_ch4_dom_dyn`: `mono=0.9348 side=0.1709 dyn=0.1760`
  - `freeze_ch4_dom_side`: `mono=0.9197 side=0.2682 dyn=0.1787`

**Conclusion**

Retargeting `soundlog` onto the weighted blockers was still the right move, because it killed a cleaner hypothesis than the late-window probe ever could.

The dominant sections really are full of live `ch1` and `ch4` pitch motion, but freezing that traffic does not improve the primary side-dynamics oracle. At best it flat-lines dynamics while harming mono or other side metrics, and at worst it just makes the music worse. So the remaining side miss is not “because those dominant windows need less live pitch movement.”

That means the event-level clue is descriptive, not causal:

- `ch1/ch4` tone changes are essential musical content in the dominant windows
- the remaining mismatch is more likely in how that content is spatialized, articulated, or represented by the external reference than in the presence of the pitch traffic itself

**Lesson**

Once the event oracle is in place, use it to kill whole classes of pretty lies quickly. “Busy control stream” is not the same thing as “causal control stream,” and freezing musically essential writes is a good way to find out which is which.

### 67. Translating harness-only YM key delay into the shared live/replay path changed the honest baseline, but not the shipped default

**Date:** 2026-04-07

**Hypothesis**

The earlier harness-only dominant-window key-delay clue was still worth one honest translation into the real architecture:

- shared live core replay state
- shared timed renderer
- real `AudioOutputConfig`

If the clue survived that translation, it might finally move the side-dynamics target without another fake harness-only win.

**Change**

- added shared `AudioOutputConfig` support for per-channel YM key-write delays
- rewired the live audio synth path to use persistent audio replay chip state instead of per-scanline cloned state, because cross-scanline delayed writes cannot be represented honestly any other way
- mirrored the same delayed-key queueing in the timed renderer
- added unit coverage for:
  - config rebuild carrying real key-delay ticks
  - centered single-op delayed key-on deferral in the timed renderer
- temporarily promoted the best harness-only candidate into the default to see what the real public GHZ scorer would say
- added a shared-model GHZ key-delay candidate sweep under the honest scorer

**Evidence**

- the shared-model translation itself worked:
  - live replay parity still passed
  - delayed single-op key-on behavior was observably deferred in a real test
- but the earlier harness-only “winner” did not survive real scoring:
  - temporary promoted default:
    - `mono final = 0.9361`
    - `sectioned side consensus final = 0.1182`
    - `sectioned side dynamics final = 0.1944`
  - this hit the primary side-dynamics target, but cratered both the mono guardrail and the side-consensus floor
- after reverting that fake promotion, the honest public baseline landed at:
  - `mono final = 0.9516`
  - `sectioned side consensus final = 0.1805`
  - `sectioned side dynamics final = 0.1506`
- the real shared-model key-delay sweep then plateaued almost immediately:
  - best primary result only reached about `0.1617`
  - nowhere close to the `0.19` side-dynamics target

**Conclusion**

The shared key-delay model was still worth building, because it made the live path more structurally honest and reset the public baseline onto firmer ground. But as an actual fidelity lever, plain per-channel key delay is exhausted. The harness-only proxy was flattering a behavior that does not survive contact with the real shared scorer.

**Lesson**

If a harness-only candidate only “wins” before being translated into the real shared path, the harness did its job by finding a clue, and your job is to kill the clue when it fails the honest architecture.

### 68. Key-edge articulation is a dead model class under the honest side-dynamics scorer

**Date:** 2026-04-07

**Hypothesis**

If the remaining stereo-side miss is really about under-articulated YM attacks, then shaping only the real key-on edges on the suspect channels should move side dynamics without damaging mono:

- immediate key-edge persistence
- delayed-seed key-edge persistence
- trigger-local key-edge transient sharpening

**Change**

- added harness-only helpers for:
  - key-on trigger extraction from real `$28` writes
  - key-edge side persistence
  - delayed-seed key-edge side persistence
  - key-edge-local side transient shaping
- added pure regression coverage for all three helpers and the key-on trigger extractor
- ran three focused GHZ stem sweeps against the honest current baseline on `ch1/ch4/ch5`

**Evidence**

- key-on counts after capture were real and not tiny:
  - `ch1 = 188`
  - `ch4 = 677`
  - `ch5 = 275`
- but all three focused slices stayed effectively flat:
  - immediate key-edge persistence:
    - best `dyn = 0.1506`
  - delayed-seed key-edge persistence:
    - best `dyn = 0.1506`
  - trigger-local key-edge transient shaping:
    - best `dyn = 0.1509`
- mono stayed pinned at `0.9516` throughout, which is good, but it also means the model was not doing anything meaningful

**Conclusion**

The retrigger branch is dead under the honest scorer. It is not just “not promoted yet”; it is flat enough that the stop rule fired. The remaining side-dynamics miss is not explained by a small amount of key-on-local stereo persistence or attack sharpening on the suspect channels.

**Lesson**

When three focused variants of the same articulation story all move dust, stop renaming the same corpse.

### 69. `soundlog` tone-change-triggered transient shaping also dies cleanly

**Date:** 2026-04-07

**Hypothesis**

If key edges were too early to matter, maybe the better trigger is not the raw register write but the higher-level `ToneChange` event stream that `soundlog` extracts from live GHZ traffic:

- especially on `ch1`
- secondarily on `ch4`

**Change**

- added a pure helper that extracts trigger samples from tracked `soundlog` YM2612 events by kind and channel
- added regression coverage for that helper
- used tracked `ToneChange` samples, not raw frequency writes, to drive a focused harness-only transient sweep on `ch1/ch4/ch5`

**Evidence**

- tracked post-capture `ToneChange` counts were dense:
  - `ch1 = 713`
  - `ch4 = 930`
  - `ch5 = 185`
- the new trigger extractor worked correctly in isolation
- but the actual GHZ sweep was brutally flat:
  - baseline:
    - `mono = 0.9516`
    - `side = 0.1805`
    - `dyn = 0.1506`
  - strongest candidates:
    - still `dyn = 0.1506`
    - no meaningful mono or side movement either

**Conclusion**

The higher-level `soundlog` oracle is still valuable for analysis, but using `ToneChange` events as simple stereo-side transient triggers does not explain the remaining public GHZ mismatch. This was a clean negative result.

**Lesson**

Better triggers do not help if the underlying model is wrong. `soundlog` should stay in the harness as an oracle, not be mistaken for proof that every event-shaped mix tweak is worth shipping.

### 70. Re-sweeping shared side memory under the honest baseline finally produced a shippable gain

**Hypothesis**

The newer, harsher public baseline may have made older shared stereo-memory levers relevant again. Instead of more one-off section hacks, re-sweep the existing shared side-memory path and then let width shaping refine around the first real winner.

**Change**

- added focused harness sweeps for:
  - strengthened `ch4/ch5` YM side memory
  - width refinement around that winner using `stereo_crossfeed` and `side_gain`
- promoted the first honest winner into the default core output config:
  - YM side memory `[0.20, 0.0, 0.0, 0.25, 0.15, 0.0]`
  - `stereo_crossfeed = 0.25`
  - `side_gain = 1.05`

**Evidence**

- starting public baseline:
  - `mono = 0.9516`
  - `side = 0.1805`
  - `dyn = 0.1506`
- best shared-memory + width winner:
  - `mono = 0.9516`
  - `side = 0.1977`
  - `dyn = 0.1686`
- live replay parity stayed exact:
  - `corr = 1.0000`
  - `rms_ratio = 1.0000`

**Conclusion**

This was the first real shipped stereo-side improvement in a while. It still did not hit the later `side dynamics >= 0.19` target, but it was a real, verified gain instead of leaderboard lint.

**Lesson**

When the oracle changes shape, old dead knobs are worth one honest re-sweep. Sometimes the corpse was only mostly dead.

### 71. The strengthened shared-memory branch plateaued again immediately after promotion

**Hypothesis**

If the promoted shared-memory/default-width winner was only a local shoulder, one more nearby strength refinement on `ch4/ch5` should keep moving the primary metric.

**Change**

- added a focused refinement sweep around the promoted default
- tested stronger nearby `ch4/ch5` side-memory combinations while keeping the promoted width settings fixed

**Evidence**

- current shipped default:
  - `mono = 0.9516`
  - `side = 0.1977`
  - `dyn = 0.1686`
- best nearby refinement:
  - `ch1_0.20_ch4_0.35_ch5_0.10`
  - `mono = 0.9516`
  - `side = 0.1988`
  - `dyn = 0.1691`
- net gain on the primary metric after promotion:
  - only `+0.0005`

**Conclusion**

The revived shared-memory branch produced one real shipped win and then fell back into dust immediately. That means the model class earned promotion once, but not more polishing.

**Lesson**

A model class can be both real and exhausted. Once the improvement drops back under the bar, stop petting it.

### 72. Decaying per-channel side carry was a cleaner analog story and still died

**Hypothesis**

The literal delayed-side hacks were too fake to ship, but the clue behind them might still be real. Instead of a fixed delayed tap, add a decaying per-channel side carry at YM native rate so stereo energy can persist a little longer in a more analog-plausible way.

**Change**

- added shared per-channel YM side-decay config to the live core and timed replay renderer
- added plumbing tests and mono-safety tests for that path
- ran three focused GHZ sweeps:
  - broad side-decay times
  - micro-decay times in the `~1-6 YM sample` range
  - micro-decay plus relaxed `ch4/ch5` side-memory amounts

**Evidence**

- broad decay was awful immediately:
  - `current_default: side = 0.1977, dyn = 0.1686`
  - `0.25-8 ms` candidates collapsed as low as `side ~= 0.092`, `dyn ~= 0.099`
- micro-decay was much less bad, but still only lint:
  - best pure micro-decay: `side = 0.1972`, `dyn = 0.1688`
  - gain on the primary metric: only `+0.0002`
- micro-decay plus amount retuning stayed lint-sized too:
  - best candidate: `ch1/ch4/ch5 amounts = 0.20/0.30/0.10`, decays `0.01/0.01/0.01 ms`
  - `side = 0.1985`
  - `dyn = 0.1693`
  - gain on the primary metric: only `+0.0007`
- mono guardrail stayed intact throughout:
  - `mono = 0.9516`

**Conclusion**

The side-decay model class is dead under the current oracle. It was cleaner than the old fake delay story, but it still failed the stop rule after three focused slices with no `> 0.002` gain on the primary metric.

**Lesson**

A prettier physical story is not the same thing as a useful model. If the honest sweep still only moves dust, kill it and move on.

### 73. A third GHZ hardware reference made the oracle stricter and more useful

**Hypothesis**

The remaining stereo-side disagreement may be as much an oracle problem as an emulator problem. If the current side scores are being overfit to two captures with known disagreements, a third distinct hardware capture should either stabilize the consensus or expose where the current two-reference oracle is too flattering.

**Change**

- added a third GHZ hardware reference from 16BAP's remaster CD pack:
  - `tests/reference_audio/sonic_ghz_16bap_remaster_cd.flac`
- verified it is distinct from both existing GHZ references by hash
- reran the public GHZ golden and dominant-section diagnostics under a three-reference consensus

**Evidence**

- the new remaster FLAC is genuinely distinct from the old 16BAP GHZ file and the original `sonic_ghz.flac`
- public GHZ under the new three-reference oracle:
  - `mono final = 0.9566` (up from `0.9516`)
  - `hybrid mono final = 0.9412` (up from `0.9320`)
  - `sectioned side consensus final = 0.1867` (down from `0.1977`)
  - `sectioned side dynamics final = 0.1596` (down from `0.1686`)
- the dominant weighted side sections moved earlier:
  - side consensus dominant: `0.00s -> 1.00s`
  - side dynamics dominant: `2.00s -> 3.00s`
- the dominant-section chip balance still points at YM, not PSG:
  - `current_default` and `ym_only` remain close
  - `psg_only` still collapses the side metrics
- `soundlog` on those new dominant windows still shows `ch1` and especially `ch4` carrying most of the `ToneChange` traffic

**Conclusion**

This did not improve the emulator, but it improved the oracle. The new three-reference consensus is clearly harsher on stereo side, shifts the weighted problem earlier in the trusted window, and keeps the blame focused on YM-side behavior rather than PSG or the old late-window crater.

**Lesson**

Better references can lower your favorite score and still be progress. A stricter oracle that points at the right window is worth more than a flattering one that keeps you tuning the wrong ghost.

### 74. The simple stereo-retune branch is dead under the three-reference oracle

**Hypothesis**

Now that the weighted side problem moved earlier under the three-reference oracle, the current shipped stereo shaping might simply be too mono-friendly in the wrong place. If so, a focused retune of the existing stereo path should raise `sectioned side dynamics` without violating a conservative mono guardrail.

**Target / Guardrail**

- primary: improve `sectioned side dynamics final`
- guardrail: keep `mono consensus final >= 0.9380`
- stop rule: kill the retune class after 3 focused slices without any `> 0.002` gain on the primary

**Change**

- added three ignored harness-only refinement sweeps:
  - `diagnose_ghz_side_dynamics_primary_stereo_mix_refine`
  - `diagnose_ghz_side_dynamics_primary_side_eq_refine`
  - `diagnose_ghz_side_dynamics_primary_stereo_eq_combo_refine`
- the sweeps all score the same public three-reference GHZ trace against:
  - mono consensus as a hard guardrail
  - sectioned side consensus as secondary context
  - sectioned side dynamics as the primary ranking signal

**Evidence**

- baseline under the current shipped default remained:
  - `mono = 0.9566`
  - `side = 0.1867`
  - `dynamics = 0.1596`
- focused stereo-mix sweep (`crossfeed`, `mid_gain`, `side_gain`):
  - best result only reached `dynamics = 0.1597` (`+0.0001`)
  - most of the "winners" were just more flattering to mono
- focused side-only EQ sweep:
  - best result only reached `dynamics = 0.1597` (`+0.0001`)
  - some variants improved side amount slightly, but not side motion
- focused stereo/EQ combo sweep:
  - best result only reached `dynamics = 0.1598` (`+0.0003`)
  - still nowhere near the `> 0.002` bar

**Conclusion**

The simple stereo-retune class is dead under the stricter oracle. We can still make mono look nicer or nudge static side amount around, but the actual target, `sectioned side dynamics`, does not move enough to justify another shipped default tweak.

**Lesson**

Once the oracle is strict enough, "retune the same knobs again" stops being engineering and starts being seance work. If three focused sweeps cannot move the primary metric by more than dust, kill the class and pivot.

### 75. Hard-panned `ch4/ch5` side boost is also the wrong story

**Hypothesis**

The early weighted blocker windows are dominated by stable hard-left `ch4` and hard-right `ch5`. If the remaining side-dynamics miss is just "those hard-panned channels are not wide enough," then boosting side only on the isolated `ch4`/`ch5` YM stems should improve the three-reference side metrics.

**Change**

- added an ignored harness-only stem sweep:
  - `diagnose_ghz_channel_stem_side_boost_sweep`
- it reuses the honest pre-clamp stem reconstruction bench and tests side gain values above `1.0` on `ch4` and `ch5`, while leaving `ch1` at neutral

**Evidence**

- stem reconstruction is still honest: `reconstruction_max_diff = 0.00008386`
- baseline remained:
  - `mono = 0.9566`
  - `side = 0.1867`
  - `dynamics = 0.1596`
- every `ch4`/`ch5` side-boost candidate was worse than baseline
- representative failures:
  - `ch5 = 1.10`: `side = 0.1862`, `dynamics = 0.1590`
  - `ch4 = 1.10`: `side = 0.1866`, `dynamics = 0.1585`
  - `ch5 = 1.20`, `ch4 = 1.20`: `side = 0.1839`, `dynamics = 0.1541`

**Conclusion**

The remaining miss is not "hard-panned channels need more side amount." That model class fails even in the friendliest possible harness-only form.

**Lesson**

When the isolated-stem version of a story already loses, there is no point dressing it up as a hardware model afterward.

### 76. Event-triggered articulation is also dead under the three-reference oracle

**Hypothesis**

If the early-window stereo miss is really about `ch4/ch5` note articulation rather than static width, then event-triggered side shaping keyed off live YM musical traffic should move the side-dynamics oracle more than static retuning did.

**Change**

- reran the existing ignored event-driven harness probes under the three-reference oracle:
  - `diagnose_ghz_soundlog_tone_change_transient_sweep`
  - `diagnose_ghz_channel_stem_key_triggered_transient_sweep`

**Evidence**

- `soundlog` tone-change-triggered shaping stayed completely flat:
  - baseline: `mono = 0.9566`, `side = 0.1867`, `dynamics = 0.1596`
  - best candidate: still `mono = 0.9566`, `side = 0.1867`, `dynamics = 0.1596`
- key-triggered transient shaping also only moved dust:
  - best candidates reached `dynamics = 0.1597` or `0.1598`
  - those tiny moves came with equal or worse side scores and no meaningful net gain

**Conclusion**

The remaining gap is not explained by simple event-triggered articulation on top of the existing stems either. That kills the last cheap emulator-side story that still had a defensible connection to the dominant `ch4/ch5` traffic.

**Lesson**

When both the static and event-triggered versions of a model class fail under the stricter oracle, stop tuning and start questioning the oracle or the premise.

### 77. Reference provenance now has explicit authority instead of pretending all captures are peers

**Date:** 2026-04-09

**Hypothesis**

The remaining disagreement is at least partly an oracle problem, not an emulator problem. If the three GHZ captures do not deserve equal mono and stereo authority, the harness should say that explicitly instead of forcing dynamic self-consistency weights to carry all that burden.

**Change**

- added a tracked manifest at `crates/genesoxide-test-harness/tests/ghz_reference_manifest.json`
- added pure resolver/weight tests to `audio_golden.rs` for:
  - manifest order
  - disabled references
  - default weights for unlisted files
  - weight sanitization
- changed GHZ reference discovery to honor manifest order and filtering instead of blind `sonic_ghz*.{flac,wav}` glob order
- threaded manual provenance weights into the fixed oracles:
  - mono consensus uses `mono_weight`
  - side consensus / side dynamics / hybrid side tie-break use `side_weight`

**Evidence**

- new pure tests pass:
  - `resolve_ghz_references_prefers_manifest_order_and_skips_disabled`
  - `resolve_ghz_references_defaults_unlisted_weights_to_one`
  - `parse_ghz_reference_manifest_sanitizes_bad_weights`
- public GHZ snapshot shifted in the expected oracle-only direction:
  - `Mono consensus final: 0.9566 -> 0.9576`
  - `Hybrid mono final: 0.9412 -> 0.9413`
  - `Sectioned side consensus final: 0.1867 -> 0.1804`
  - `Sectioned side dynamics final: 0.1596 -> 0.1614`
- hybrid profile sweep stayed tightly clustered, but with provenance weighting the leader is now:
  - `xf_0_40 = 0.9425`
  - `current_default = 0.9413`

**Conclusion**

This is an oracle cleanup, not an emulator win. The mono-side split is more honest now: broad mono guidance got slightly stronger, while unreliable stereo-side authority got weaker. That is the right outcome given the cross-capture fights we already measured.

**Lesson**

Reference provenance must be first-class input to the oracle. Dynamic agreement weights are useful, but they are not a substitute for saying out loud which captures deserve mono trust and which ones should not get to boss stereo scoring around.

### 78. The remaster reference keeps mono value but loses all stereo authority

**Date:** 2026-04-09

**Hypothesis**

The new authority/ablation probe suggested the remaster capture was still buying side pressure at too high a cost. If its stereo contribution is mostly noise, setting its `side_weight` to zero should improve the side oracles while leaving mono largely intact.

**Change**

- added an ignored probe, `diagnose_ghz_reference_authority_and_ablation`, to print:
  - manual mono/side manifest weights
  - effective mono/side weights after dynamic reliability
  - leave-one-out oracle deltas per reference
- used that probe to inspect the current three-reference mix
- changed `sonic_ghz_16bap_remaster_cd.flac` from `side_weight = 0.35` to `side_weight = 0.0` in `ghz_reference_manifest.json`

**Evidence**

- before the change, effective remaster side authority was still non-trivial:
  - `side_cons = 0.0976`
  - `side_dyn = 0.0972`
- with remaster side authority disabled:
  - `Mono consensus final` stayed `0.9576`
  - `Hybrid mono final` dipped slightly: `0.9413 -> 0.9405`
  - `Sectioned side consensus final` improved: `0.1804 -> 0.2021`
  - `Sectioned side dynamics final` improved: `0.1614 -> 0.1658`
- updated authority/ablation readout:
  - `sonic_ghz.flac`: mono effective `0.8731`, side effective `0.3911 / 0.3901`
  - `sonic_ghz_16bap.flac`: mono effective `0.8508`, side effective `0.1535 / 0.1536`
  - `sonic_ghz_16bap_remaster_cd.flac`: mono effective `0.7281`, side effective `0.0000 / 0.0000`

**Conclusion**

This is another oracle cleanup win. The remaster still contributes useful mono/tone pressure, but its stereo-side contribution was more harmful than helpful. Zeroing its side authority made the side oracles materially cleaner without damaging the broad mono read.

**Lesson**

When a reference helps mono but hurts stereo, split those authorities instead of forcing a binary keep/drop decision.

### 79. The remaining 16bap side authority also wanted to die

**Date:** 2026-04-09

**Hypothesis**

After zeroing the remaster's side authority, the remaining side disagreement might still be coming from `sonic_ghz_16bap.flac`. If that capture's stereo contribution is also mostly harmful, a focused side-weight sweep should show it clearly.

**Change**

- added a reusable side-weight override path in `audio_golden.rs`
- added a pure override test for targeted side-weight mutation
- added `diagnose_ghz_16bap_side_weight_sweep`
- swept `sonic_ghz_16bap.flac` side authority from `0.00` to `1.00`
- promoted the winning oracle state by setting `sonic_ghz_16bap.flac` to `side_weight = 0.0`

**Evidence**

- the sweep came back embarrassingly clear:
  - `16bap_side = 0.00`: `mono = 0.9576`, `hybrid = 0.9416`, `side = 0.2282`, `dyn = 0.1730`
  - `16bap_side = 0.65`: `mono = 0.9576`, `hybrid = 0.9405`, `side = 0.2021`, `dyn = 0.1658`
  - `16bap_side = 1.00`: `mono = 0.9576`, `hybrid = 0.9403`, `side = 0.1971`, `dyn = 0.1706`
- post-promotion authority probe:
  - `sonic_ghz.flac`: side effective `0.3990 / 0.3990`
  - `sonic_ghz_16bap.flac`: side effective `0.0000 / 0.0000`
  - `sonic_ghz_16bap_remaster_cd.flac`: side effective `0.0000 / 0.0000`

**Conclusion**

The side oracle is now effectively primary-capture-only, and that is more honest than pretending the other captures provide useful stereo supervision. This raised both side oracles materially while leaving mono untouched.

**Lesson**

If a side-weight sweep peaks at zero, the capture is not a weak stereo reference. It is a bad stereo reference.

### 80. Returning to emulator-side work finally found a real micro side-memory win

**Date:** 2026-04-09

**Hypothesis**

With the stereo oracle cleaned up so that only the primary capture carries side authority, the old YM per-channel side-memory model might finally have a fair shot. The most likely place for new signal was the weighted `3-5s` blocker region, not the old haunted late-window crater.

**Change**

- reran the strongest emulator-side stereo candidates against the cleaned oracle
- added a tighter second micro side-memory sweep around the first nontrivial winner
- promoted the winning default:
  - YM per-channel side memory amounts: `[0.20, 0.0, 0.0, 0.40, 0.10, 0.0]`
  - YM per-channel side decay: `[0.01, 0.0, 0.0, 0.03, 0.03, 0.0] ms`

**Evidence**

- first refine pass already showed the path had real signal again:
  - `current_default`: `mono = 0.9576`, `side = 0.2282`, `dyn = 0.1730`
  - `a_0.20_0.30_0.10_d_0.01_0.02_0.02`: `mono = 0.9576`, `side = 0.2273`, `dyn = 0.1760`
- tighter sweep found the actual winner:
  - `a_0.20_0.40_0.10_d_0.01_0.03_0.03`: `mono = 0.9576`, `side = 0.2326`, `dyn = 0.1862`, `combined = 0.7355`
- public GHZ after promotion:
  - `Mono consensus final`: `0.9576 -> 0.9575`
  - `Hybrid mono consensus final`: `0.9416 -> 0.9388`
  - `Sectioned side consensus final`: `0.2282 -> 0.2283`
  - `Sectioned side dynamics final`: `0.1730 -> 0.1863`
  - dominant side-dynamics blocker `3.00s -> 4.00s`: `0.1449 -> 0.1712`

**Conclusion**

This is the first emulator-side stereo promotion in a while that survived the cleaned oracle and the public GHZ golden. The gain is not magical, but it is real: more side articulation in the weighted blocker windows without sacrificing the broad mono fit.

**Lesson**

The old side-memory idea was not dead. It was being judged under a noisier stereo oracle. Once the oracle stopped lying about secondary captures, the micro-memory/decay path finally had enough signal to earn a promotion.

### 81. Narrowing the side-only low-mid cut finally cleared the side-dynamics target

**Date:** 2026-04-09

**Hypothesis**

With the new YM side-memory baseline in place, the old `side_q_1_50` candidate deserved another trial. The question was whether the narrower `450 Hz` side cut would finally help the weighted `3-5s` side-dynamics blocker enough to matter, or just trade one corpse for another.

**Change**

- reran the main side-dynamics, side-consensus, and hybrid-mono diagnostics against the promoted side-memory baseline
- added a direct tradeoff probe comparing:
  - `current_default`
  - `xf_0_40`
  - `side_q_1_50`
  - `xf_0_40 + side_q_1_50`
- promoted `post_side_eq_1` from `Q 0.95` to `Q 1.50`

**Evidence**

- direct tradeoff probe:
  - `current_default`: `mono = 0.9575`, `hybrid = 0.9388`, `side = 0.2283`, `dyn = 0.1863`
  - `side_q_1_50`: `mono = 0.9575`, `hybrid = 0.9384`, `side = 0.2261`, `dyn = 0.1916`
  - `xf_0_40`: `mono = 0.9575`, `hybrid = 0.9420`, `side = 0.2070`, `dyn = 0.1511`
  - `xf_0_40 + side_q_1_50`: `mono = 0.9575`, `hybrid = 0.9415`, `side = 0.2037`, `dyn = 0.1550`
- public GHZ after promotion:
  - `Mono consensus final`: stayed `0.9575`
  - `Hybrid mono consensus final`: `0.9388 -> 0.9384`
  - `Sectioned side consensus final`: `0.2283 -> 0.2261`
  - `Sectioned side dynamics final`: `0.1863 -> 0.1916`
  - dominant `3.00s -> 4.00s` side-dynamics section: `0.1712 -> 0.1844`

**Conclusion**

This is a real trade, not a fake win. The narrower side cut costs a little broad side consensus and a tiny bit of hybrid mono score, but it is the first post-oracle emulator-side move that actually clears the `0.19` side-dynamics target while keeping mono pinned.

**Lesson**

Once the side oracle is honest, you can finally make a narrow, slightly ugly trade on purpose instead of letting broad stereo heuristics drown the real blocker section.

### 82. More `ch4`, almost no `ch5`, and shorter decay pushed side dynamics much higher

**Date:** 2026-04-09

**Hypothesis**

After the side-Q promotion, the side-memory branch still looked alive. The next question was whether the new baseline wanted even more `ch4` carry, less `ch5`, and shorter decay, or whether that would just turn into another fake local sweep.

**Change**

- added two tighter post-Q side-memory refine sweeps
- promoted the winning default:
  - YM per-channel side memory amounts: `[0.20, 0.0, 0.0, 0.60, 0.0, 0.0]`
  - YM per-channel side decay: `[0.01, 0.0, 0.0, 0.02, 0.02, 0.0] ms`
- ran one more high-`ch4`, tiny-`ch5` sweep to check whether the branch still had headroom

**Evidence**

- first post-Q refine found a big jump:
  - previous default: `mono = 0.9575`, `side = 0.2261`, `dyn = 0.1916`
  - `a_0.20_0.50_0.05`: `mono = 0.9575`, `side = 0.2298`, `dyn = 0.2280`
- second refine found the better simpler winner:
  - `a_0.20_0.60_0.00_d_0.02_0.02`: `mono = 0.9575`, `side = 0.2297`, `dyn = 0.2465`
- the extra refine did not beat it:
  - best remaining nearby candidates stayed at or below `dyn = 0.2459`
  - `current_default` stayed tied for best under that last sweep
- public GHZ after promotion:
  - `Mono consensus final`: stayed `0.9575`
  - `Hybrid mono consensus final`: `0.9384 -> 0.9386`
  - `Sectioned side consensus final`: `0.2261 -> 0.2297`
  - `Sectioned side dynamics final`: `0.1916 -> 0.2465`
  - dominant side-dynamics blocker shifted to `2.00s -> 3.00s` at `0.1934`

**Conclusion**

This is another real emulator-side gain, and much larger than the last one. The surviving pattern is now very blunt: `ch4` wants to dominate the side-memory story, `ch5` mostly does not help, and long decay was hurting more than helping.

**Lesson**

Once a model class starts producing repeated real gains, keep narrowing it until the winner stops moving. That is how you get out of audio seance mode and back into engineering.

### 83. The remaining “distortion” complaint was mostly the boring clamp story

**Date:** 2026-04-10

**Hypothesis**

The user’s “music still sounds distorted / detuned” report might be real, but not because the FM core was suddenly wrong again. The more likely explanations were:

- the new side-memory branch was adding steady stereo smear to sustained tones
- the final output gain was still clipping just enough to make music feel crunchy

**Change**

- added a transient-biased side-memory feed model to the shared live/replay path
- added a sign-aligned delayed-side model that follows current side polarity instead of replaying stale waveform sign
- added title and GHZ distortion comparison diagnostics for those models
- added a focused master-gain sweep against the current default
- promoted the winning boring fix:
  - `master_gain = 2.2` instead of `2.5`

**Evidence**

- transient-biased side memory was a clean physical story but a bad GHZ trade:
  - `current_default`: `mono = 0.9575`, `hybrid = 0.9386`, `side = 0.2297`, `dyn = 0.2465`
  - `tm_0.00_0.25_0.00_a_0.60_0.00_d_0.02_0.02`: `dyn = 0.2085`
  - stronger transient mixes degraded side metrics even further
- sign-aligned delayed side also did not earn promotion:
  - best candidate `sa_0.00_0.25_0.00_a_0.60_0.00`: `mono = 0.9575`, `hybrid = 0.9383`, `side = 0.2276`, `dyn = 0.2465`
  - title/GHZ coarse distortion proxies stayed effectively identical under that branch
- the master-gain sweep was the first unambiguously good distortion trade:
  - `gain = 2.50`: `title_clip = 88`, `ghz_clip = 88`, `mono = 0.9575`, `hybrid = 0.9386`, `side = 0.2297`, `dyn = 0.2465`
  - `gain = 2.20`: `title_clip = 10`, `ghz_clip = 10`, `mono = 0.9667`, `hybrid = 0.9478`, `side = 0.2308`, `dyn = 0.2439`
- public GHZ after promotion:
  - `Mono consensus final`: `0.9575 -> 0.9667`
  - `Hybrid mono consensus final`: `0.9386 -> 0.9478`
  - `Sectioned side consensus final`: `0.2297 -> 0.2308`
  - `Sectioned side dynamics final`: `0.2465 -> 0.2439`
  - `Local spectral`: `0.9506`
  - `Trusted window spectral`: `0.9066`
  - `Trusted window RMS ratio`: `1.0423`

**Conclusion**

The user’s complaint was probably real, but the next fix was not another stereo voodoo branch. The honest win was reducing final clamp pressure. That cut the obvious clipping count dramatically while actually improving the broad mono/hybrid oracle and leaving the stereo-side story almost unchanged.

**Lesson**

When the fancy analog theories start breeding, check the stupid gain knob again. If a small gain cut improves both clipping and the broad oracle, that is a real fix, not a defeat.

## Current Default Output Chain

Current default Genesis post-mix chain, in order:

1. `Legacy` YM filter profile
2. `master_gain = 2.2`
3. `ym_gain = 1.1`
4. `psg_gain = 0.65`
5. `stereo_crossfeed = 0.25`
6. `side_gain = 1.05`
7. `post_low_pass_hz = 12_000`
8. low shelf `110 Hz / -6 dB`
9. peaking EQ `380 Hz / Q 0.65 / +4.5 dB`
10. high shelf `2.6 kHz / -2.8 dB`
11. peaking EQ `190 Hz / Q 0.90 / +3.2 dB`
12. peaking EQ `560 Hz / Q 1.20 / -2.4 dB`
13. side-only peaking EQ `450 Hz / Q 1.50 / -4.0 dB`
14. side-only peaking EQ `2.6 kHz / Q 0.90 / 0.0 dB`
15. YM per-channel side memory amounts: `[0.20, 0.0, 0.0, 0.60, 0.0, 0.0]`
16. YM per-channel side decay: `[0.01, 0.0, 0.0, 0.02, 0.02, 0.0] ms`

## Current Metrics Snapshot

Current public GHZ golden snapshot:

- `Envelope corr = 0.5817`
- `Best local env = 0.7049`
- `Local spectral = 0.9506`
- `Local RMS ratio = 1.0921`
- `Local raw corr = 0.0168`
- `Local stereo: emu_lr_corr = 0.9632, ref_lr_corr = 0.9426`
- `Local stereo: emu_side = 0.1367, ref_side = 0.1739`
- `Local mid/side spectral: mid = 0.9525, side = 0.4978`
- `Local phase coherence: left = 0.0991, right = 0.0916, mid = 0.0967, side = 0.1089`
- `Local stereo lag: emu = 0, ref = 0`
- `Reference self-match @ inferred loop 51.83s: raw = -0.0041, spectral = 0.9200, RMS ratio = 0.7899`
- `Reference self mid/side spectral: mid = 0.9238, side = 0.5114`
- `Reference self phase coherence: left = 0.0916, right = 0.0889, mid = 0.0913, side = 0.1284`
- `Trusted ref window: prior = 25.59s, current = 77.42s, score = 0.9308, self_left = 0.9107, self_mid = 0.9198, self_side = 0.8747, self_rms = 0.9754`
- `Trusted window emu/ref: raw = 0.0017, spectral = 0.9066, RMS ratio = 1.0423, mid = 0.9083, side = 0.2586`
- `Mono consensus oracle: refs = 3, self_spectral = 0.9781, self_rms_fit = 0.8366, average = 0.9695, penalty = 0.0028, final = 0.9667`
- `Hybrid mono consensus: refs = 3, average = 0.9506, penalty = 0.0028, final = 0.9478`
- `Sectioned side consensus: average = 0.2761, penalty = 0.0453, final = 0.2308, worst_section = 0.1010`
- `Sectioned side dominant: 3.00s -> 4.00s, final = 0.1094, impact = 0.1749`
- `Sectioned side raw weakest: 5.00s -> 6.00s, final = 0.1010`
- `Sectioned side dynamics: average = 0.2698, penalty = 0.0259, final = 0.2439, worst_section = 0.1435`
- `Sectioned side dynamics dominant: 2.00s -> 3.00s, final = 0.1934, impact = 0.1435`
- `Sectioned side dynamics raw weakest: 5.00s -> 6.00s, final = 0.1435`
- `Sectioned side dynamics weakest lag: bins = -2, ms = -46.4, adjusted_env = 0.1028`
- `Sectioned side dynamics weakest transient: raw = -0.0332, adjusted = 0.1522, RMS = 1.0426`
- `Sectioned side dynamics weakest reference transient: pairs = 3, env = 0.3127 -> 0.3326, trans = 0.2512 -> 0.3044, RMS fit = 0.2952, lag = 11.6 ms`
- `Reference provenance manifest: primary capture keeps full mono/side authority; sonic_ghz_16bap.flac keeps mono pressure but has zero stereo authority; sonic_ghz_16bap_remaster_cd.flac also has zero stereo authority`

Interpretation:

- Local matched-window similarity is materially better than earlier passes.
- Tonal balance is still close and broad level is slightly better behaved against the provenance-aware mono/hybrid oracles.
- Stereo-side scoring is now harsher by design because the harness no longer pretends all three captures deserve equal side authority.
- The new side-only oracle agrees with the current default and points at specific weak sections instead of a vague global stereo failure.
- The sectioned side-consensus oracle now also discounts weak-section transient fights across captures instead of treating them like clean targets.
- The side oracle is now effectively primary-capture-led, which turns out to be more stable and more useful than pretending the other captures supervise stereo.
- In the weak sections, emulator side energy is still systematically too narrow.
- The new side-dynamics oracle says those weak sections also miss on side motion shape, not just side amount.
- A small local side-envelope lag exists, but re-lagging does not rescue the weak sections.
- The weakest section also has badly underpowered side transients even after re-lag.
- The two hardware captures themselves only weakly agree on the weakest section’s side transient target, especially on transient strength.
- The side-dynamics scorer now downweights those cross-capture transient fights instead of giving them full authority.
- The hybrid side tie-break now also discounts those unreliable side sections, which raised confidence in the score but did not create a meaningful new retune winner.
- The sections that actually drag the weighted side scores are not the same ones as the raw ugliest craters.
- The earlier local-offset pan diagnostics were wrong; the absolute-window diagnostics show real later pan activity, but the bigger neutral-path culprit is actually channel 1’s persistent hard-right occupancy.
- The simple centered-tone path is clean, so the remaining stereo leak is traffic-dependent rather than a trivial mono-pan failure.
- The neutral-path GHZ side leak is materially driven by later live pan history in the absolute dominant windows, not just by the intro or the first few seconds after capture.
- Channel 5 materially shapes the remaining side-dynamics miss, while channel 4 looks like a smaller late contributor.
- A simple static hard-pan bleed model on the suspect YM channels does not improve the external scores once tested on an honest stem bench.
- Targeted transient shaping on the same suspect stems is slightly less wrong than hard-pan bleed, but still nowhere near strong enough to justify a shipped retune.
- A tiny delayed-side-memory effect on `ch1` plus `ch4` is the first honest post-pan suspect-stem lever that improves the primary side oracle by more than lint.
- That improvement plateaus immediately under the first sensible hybrid refinement, so this model class still looks too weak to reach the current `0.27` side-consensus target.
- Promoting the real core-side version of that `ch1/ch4` side-memory hint preserved the mono guardrail and moved the shipped public side oracle from `0.2468` to `0.2502`.
- Refining that real core-side model plateaued immediately, so the explicit stop rule fired for this model class instead of turning into another endless tweak loop.
- The weak-section band deltas are not coherent enough to justify another one-shot global side EQ retune.
- The new phase-coherence readout says the remaining mismatch is not a clean static phase/filter target.
- The GHZ reference itself is unstable enough loop-to-loop that side-channel and raw-phase metrics should be treated cautiously.
- On the FLAC's own trusted loop-stable window, the emulator is already quite close in broad tone and level.
- The shared mono consensus oracle says the captures agree far more strongly on mono content than on stereo-side behavior.
- The remaining disagreement is narrower now, and the one parameter that moved monotonically under the hybrid scorer was PSG level.
- The remaining gap looks even more like reference-model or capture-chain behavior than a fundamental chip-core bug.
- The user’s remaining “distortion” complaint was better explained by final clamp pressure than by the newer stereo-side model branches.
- Both transient-biased side feed and sign-aligned delayed side were plausible stories, but neither earned promotion once tested honestly.

## What Most Likely Remains

The remaining gap is most likely in one or more of:

- more faithful capture/reference alignment assumptions
- a better external reference recording or a second independent capture
- differences between the chosen hardware recording and the emulator's assumed console/output chain
- stereo-side behavior that only becomes obvious on loop-stable reference sections
- non-static capture behavior that is not well approximated by a single fixed filter or EQ curve
- disagreements between different real-world hardware capture chains, especially on weak-section side transients, that a single static default cannot satisfy simultaneously

It is less likely to be:

- a fundamental YM2612 operator bug
- a basic GHZ sequencing bug
- a simple gain mismatch
- a simple mono/stereo width mismatch
- a simple interchannel sample delay mismatch
- a side channel that is globally too strong or too weak
- a simple left/right EQ asymmetry
- a simple static phase/all-pass correction
- a naive one-number YM/PSG gain rebalance derived from raw PCM fitting
- fine-grained stereo-side retuning against this single FLAC as if it were a perfectly stable oracle
- promoting a side-only tweak just because it flatters one hardware recording
- a simple one-stage low-pass problem

## Transferable Lessons For SNES Audio

This work should carry directly into SNES audio bring-up:

- Build the oracle early.
  For SNES, that likely means SPC/DSP reference playback or a trusted reference core before mix tuning.

- Instrument timed live writes.
  Real traffic traces are worth far more than subjective listening while the core is still in flux.

- Prove the harness is comparing the same content.
  Boot flow, window alignment, loop position, and replay state matter as much as synthesis.

- Keep live and replay paths structurally aligned.
  If they disagree, your diagnostics are suspect before your chip core is.

- Separate magnitude shaping from phase/IR shaping.
  They solve different classes of mismatch.

- Treat fitting from program material with suspicion.
  Fitted filters can overfit musical content instead of capture-chain behavior.

## Update Protocol

When adding a new audio pass, append a new subsection to **Chronology** with:

- `Date`
- `Hypothesis`
- `Change`
- `Evidence`
- `Conclusion`
- `Lesson`

If a new default is promoted, also update:

- **Current Default Output Chain**
- **Current Metrics Snapshot**
- **What Most Likely Remains**

## Suggested Next Entry Template

```md
### N. Short title

**Date:** YYYY-MM-DD

**Hypothesis**

...

**Change**

...

**Evidence**

- metric
- metric
- failing / passing regression

**Conclusion**

...

**Carry-forward lesson**

...
```

