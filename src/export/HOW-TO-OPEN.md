# Opening the FFX PS2 music in a DAW

FFX Music Studio made this folder (`ffx-music-studio --export`) from your
own PS2 disc of Final Fantasy X. It is for your own use: don't share it.

| Folder / file | What |
|---|---|
| `midi/musicNNN-<name>.mid` | one Standard MIDI File (type 1) per sequence: a tempo/marker track, then one track per sequence channel (`chNN wdBBB bN #P`: channel, WD bank, its SF2/MIDI bank number, first instrument) |
| `soundfonts/wdBBB.sf2` | one SoundFont 2 per instrument bank that the music uses (`wave_wd.dat` bank BBB) |
| `soundfonts/ffx-ps2.sf2` | every bank in one font (see below) |
| `samples/wdBBB/NNNN.wav` | the bank's samples, 16-bit mono at their rate, root key and loop in the `smpl` chunk |
| `reverb/M-<preset>.wav` | an impulse response of each SPU2 reverb preset (32-bit float stereo) |
| `reverb.json` | per track: reverb preset, effect volume and its changes; per preset its registers |
| `tracks.json` | per track: SoundFont, bank, tempo, loop, length, channels |
| `renders/musicNNN-<name>.wav` | the PS2 sound engine's own render (intro and two passes of the loop, or once through and 2 s of tail): the reference to compare your DAW with |

## Which SoundFont goes with which MIDI file

Every sequence plays the instruments of one bank. The table at the end
lists the bank of each file. {{COMBINED}}

- **Bank numbers:** the disc's banks have numbers up to 250 or so (`wdBBB`
  is that number), more than MIDI bank select and SoundFont melodic banks
  allow (SF2 bank 128 is percussion). So the banks the music uses are
  numbered 0, 1, 2, ... in the order of their WD numbers: the "SF2/MIDI
  bank" column of the table. Each channel starts (and every program change
  repeats) with CC0 = that number and CC32 = 0, then the program change =
  the instrument number in the bank. Every font, the per-bank ones too,
  has its presets at that bank number, program = instrument number, so
  the same MIDI file selects the right preset with `wdBBB.sf2` or with
  `ffx-ps2.sf2`.
- Players that read bank select as MSB only ("GS" style, FluidSynth's
  default) find the bank. A player that ignores bank select: load
  `wdBBB.sf2` alone (it holds one bank) and pick the preset by program
  number; the preset names are `wBBBiPPP`.
- Channel 10 is never used (General MIDI drums). Sequences with more than
  15 channels that play notes reuse channels on a second MIDI port (port
  meta event); give each MIDI track its own instrument in the DAW and the
  channel numbers don't matter.

## Before you press play

- **Tempo:** the file has the driver's tempo map (48 ticks a quarter note,
  the tempo in BPM as the sequence sets it, tempo changes where it changes
  them). Import it with the tempo map; a DAW that keeps its own project
  tempo plays at the wrong speed.
- **Pitch bend range:** each channel starts with RPN 0 = 2 semitones (the
  driver's default) and sends a new RPN 0 where the sequence changes it.
  A plugin that ignores RPN: set its bend range to 2 by hand.
- **Volume:** velocity, CC7 (volume) and CC11 (expression) are linear
  amplitude factors on the PS2. The fonts carry modulators that make them
  linear in SoundFont players too (FluidSynth, Polyphone, most players
  that follow the SF2.01 spec). A player that ignores modulators plays
  quiet notes quieter than the PS2 (the SF2 default curve is the square).
  CC7 includes the song's own master volume.
- **Loop:** the MIDI holds the intro and one pass of the loop. The conductor
  track has markers `loopStart` and `loopEnd`, and every channel has CC111
  at the loop start (the "RPG Maker" loop convention). Set the DAW's loop
  range to the two markers. Counted loops inside the music (`repeat n/m`
  markers) are written out in full, as the driver plays them.
- **Reverb:** see the steps below and "Known differences".

## Step by step

### Reaper

1. *Insert > Media file...*, pick the `.mid`. Answer *Expand MIDI tracks to
   new REAPER tracks* and *Import MIDI tempo map* with yes.
2. On each new track add a SoundFont instrument (for example the free
   Juicysfplugin or sforzando, or any SF2 VSTi) and load the track's
   `soundfonts/wdBBB.sf2`. Let the plugin follow program changes, or pick
   the preset named in the track name (`ch03 wd012 b2 #5` = SF2 bank 2,
   preset 5).
3. Reverb: add a track "PS2 reverb" with *ReaVerb* (*Add > File*: the
   song's `reverb/M-<preset>.wav` from `reverb.json`; dry off, wet at
   `wet_gain`). Send the channels that have CC91 = 127 to it.
4. Loop: the markers `loopStart`/`loopEnd` are in the ruler; set the time
   selection between them and switch repeat on.

### FL Studio

1. *File > Import > MIDI file*, pick the `.mid`; in the import dialog tick
   *Start new project* and *Accept*. FL takes the tempo; tempo changes
   come in as a tempo automation clip.
2. Give each channel *Fruity SoundFont Player*, load
   `soundfonts/wdBBB.sf2` and set *Bank* and *Patch* to the bank and
   instrument of the track name (`ch03 wd012 b2 #5`: bank 2, patch 5).
3. Set the pitch bend range of each SoundFont Player to 2 (it does not read
   RPN).
4. Reverb: on a mixer insert add *Fruity Convolver*, load the song's
   `reverb/M-<preset>.wav`, 100 % wet, level at `wet_gain`; send the
   channels with CC91 = 127 to that insert.
5. Loop: loop the playlist between the two markers.

### Ableton Live

Live does not load SoundFonts and ignores a MIDI file's tempo map and
program changes, so:

1. Set the project tempo to the song's tempo (the table below; songs with
   tempo changes need tempo automation by hand, `tracks.json` has the
   ticks).
2. Drag the `.mid` into the arrangement: one MIDI track per channel.
3. On each track load an SF2 plugin (Juicysfplugin, sforzando) with
   `soundfonts/wdBBB.sf2` and the preset of the track name; set its bend
   range to 2.
4. Reverb: a return track with *Hybrid Reverb* (drag the IR WAV into its
   convolution section) or another convolution plugin, 100 % wet at
   `wet_gain`; send the channels with CC91 = 127.

### A generic SoundFont player

- **FluidSynth** (command line): render a file to WAV at the SPU2's rate
  and without FluidSynth's own effects:

  ```
  fluidsynth -ni -R 0 -C 0 -g 1 -r 48000 -F out.wav soundfonts/wd016.sf2 midi/music016-....mid
  ```

  Then convolve with the reverb IR in any editor, or compare `out.wav` with
  `renders/`.
- **Polyphone** (SF2 editor): opens the fonts to see the samples, key and
  velocity ranges, tuning and envelopes, and plays them from a MIDI
  keyboard.
- **sforzando** (standalone or plugin): loads one `.sf2` and plays the MIDI
  from your host.

## Known differences to the PS2

The PS2 plays these sequences through its sound driver (`iopsound.irx`)
on the SPU2 chip. A MIDI and SoundFont setup can't reproduce all of it:

- **Interpolation:** the SPU2 resamples with a 4-tap Gaussian filter, which
  dulls the highs a little; SoundFont players interpolate linearly or
  better (brighter). Pitches more than two octaves above a sample's rate are
  clamped on the SPU2, not in a SoundFont.
- **ADSR:** the SPU2's envelope (linear or exponential attack, exponential
  decay to a sustain level, a sustain phase that can fall or rise, linear or
  exponential release) is fitted to the SF2 envelope (linear attack; decay
  and release linear in dB). Times match at the -30 dB point; a falling
  sustain becomes one long decay to silence; a rising sustain is held.
  Envelope changes by the sequence (commands 0x30-0x39) are not in the
  MIDI.
- **Vibrato, tremolo, auto-pan (the driver's LFOs), portamento, detune and
  pitch scale** are not in the MIDI. Notes keep their timing to the
  driver's tick; the PS2 starts them on its next 240 Hz timer interrupt
  (up to 4 ms later).
- **Noise** and **pitch modulation** between voices are SPU2 features with
  no SoundFont equivalent (the driver's music does not use them).
- **Voices:** the SPU2 has 48 voices and the driver steals the quietest;
  a SoundFont player has more, so notes the PS2 cut short may ring on.
  Tied notes (legato) are separate MIDI notes.
- **Pan:** the driver's pan law comes from the disc; SF2 players use their
  own. A region with a fixed pan ignores the channel's pan on the PS2; in
  a SoundFont, CC10 still moves it. Stereo sample pairs are two zones
  panned hard left and right.
- **Reverb:** the PS2 reverb is one effect on the whole mix, its level set
  per song (some songs change it: `depth_changes` in `reverb.json`). The
  impulse response is that effect captured from a click: the SPU2 reverb
  is linear, so a convolution is close, but it runs at 24 kHz with its own
  resampling. Regions with a reverb flag send to it whatever the channel
  says, and some of them only to the reverb (no dry sound), which a
  SoundFont can't do.
- **Loop:** at its loop point the PS2 restores the positions, keys and
  tempo of the channels, not their volumes and instruments, so a second
  pass can differ slightly from the first, which is the one in the MIDI.
- **Missing banks:** ten sequences name banks that are not on the disc
  (the table says so): their MIDI is there, without a SoundFont.
- The streamed guitar and vocals of Otherworld, Confrontation and Jecht's
  theme were never part of these sequences.

## The tracks

{{TRACKS}}
