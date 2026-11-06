//! FFX PS2 Sound: a VST3/CLAP instrument playing MIDI through FFX's PS2
//! sound driver (`iopsound.irx`, ported in ffx-demaster's `sqseq`) on its
//! SPU2 model, with the instrument banks of the player's own disc.
//! `engine` is the processing, `midi` the MIDI to driver commands,
//! `source` the disc or export folder, `plugin` the nih-plug wrapper.

pub mod engine;
pub mod midi;
pub mod offline;
pub mod plugin;
pub mod source;
