# XTC and Cue Consistency: what they rest on

`XTC` and `CUE` are the two stages in AuraEngine that change where you hear a
sound rather than how it is reproduced. This page says which findings of hearing
research and acoustics each design decision rests on, what is known work that
we build on, what is our own, and how we check it. The controls and the numbers
are in [18 — DSP stages](18-dsp-stages.md#cue--cue-consistency); this page is the why.

**In short.** Neither idea is new in itself. Crosstalk cancellation has been
studied since the 1960s. Reading where a source sits in a stereo mix, and playing
it again with the cues of that direction, has been published since the mid-2000s
(Goodwin & Jot 2007 [24]; Breebaart & Schuijers 2008 [25]). What is ours is how the
two are designed and put together in a player, what CUE deliberately does *not*
do, and how both are measured. Section 5 lists it.

---

## 1. What the ear uses to place a sound

**Two cues, split by frequency.** Rayleigh's duplex theory [1]: below about
1.5 kHz the ear places a sound by the *time* difference between the ears (ITD), above
it by the *level* difference (ILD) that the head's shadow creates. The fine-structure
time difference stops being usable at about 1.4 kHz [2]. Above that, the ear still
follows a time difference in the *envelope* of a sound [3].

**One direction, one set of cues.** For a real source at an angle θ the cues
agree with each other:

- **Time.** The head is not transparent, so sound goes around it. The classic ray
  model (Woodworth) gives ITD = (a/c)(θ + sin θ), about 0.65 ms at the side for a
  head of radius a = 8.75 cm [3]. At low frequencies a sphere gives up to half as
  much again (Kuhn [4]): the wave diffracts rather than travels around as a ray.
- **Level.** A rigid sphere gives the head shadow: almost nothing at low
  frequencies, rising with frequency and angle (Rayleigh's series, in the form of
  Duda & Martens [5]).

**What a pan-pot mix gives instead.** Most studio stereo is panned by level alone:
a source differs between the channels by one level ratio, the same at every
frequency, with no time difference. Over two loudspeakers without XTC, both
speakers reach both ears, and the level difference between the speakers turns
into a time difference at the ears at low frequencies (summing localization;
Bauer [6], Blauert [3]). This is why amplitude panning works at all, and it
works best near the centre and degrades toward the sides [7, 8].

**Why it matters for XTC and headphones.** XTC removes exactly that mixing in the
air: each ear gets mostly its own channel, as on headphones. Then a panned source
arrives with a level difference at every frequency, including the bass, where no
real source has one, and with no time difference, which says "centre". The cues
contradict each other. CUE exists to remove that contradiction for the angle the
mix already gives the source. It does not invent a place.

---

## 2. XTC: what it builds on

### The known method

| Idea | Where it comes from |
|---|---|
| Cancel the far speaker at each ear with a filtered, delayed copy | Atal & Schroeder, 1960s [9]; Bauer 1961 [6] |
| "Transaural": binaural over two speakers, the shuffler form (sum and difference channels) | Cooper & Bauck [10, 11] |
| The symmetric 2×2 plant diagonalises in sum/difference (mono/side) | Cooper & Bauck [10, 11] |
| A regularised inverse instead of an exact one (Tikhonov, frequency-dependent β) | Kirkeby, Nelson et al. [12] |
| Keep the result uncoloured: flat mono path, bounded side path | in the spirit of Choueiri's spectrally uncoloured XTC [13] |
| It works for one seat; a small head movement costs much of it | Ward & Elko [14] |
| Fractional-octave smoothing of the inverse, so it has no comb | Hatziantoniou & Mourjopoulos [15] |

### Our design choices and what each rests on

- **A head, not straight lines.** The paths from each speaker to each ear come from
  a rigid sphere at your angle and distance, with the source at a finite range
  (Duda & Martens [5]). Straight lines say 382 µs between the ears at ±47°; the
  MIT KEMAR dummy head [16] measures 510–660 µs below 1 kHz and about 400 µs above
  1.4 kHz, and the sphere gives 590 and 450. Designing XTC on a sphere is known
  work too [17, 18]; we use it because it is the simplest model that gets the time
  around the head right.
- **The head's size from a tape measure.** A head's width moves its acoustic radius
  by only about a quarter of itself, the rest coming from depth and height (Algazi,
  Avendano & Duda [19]). So the sphere's radius is 45 mm plus a quarter of the width
  you enter, not half of it.
- **The ears at ±95°.** Duda & Martens fit listeners with ears at 100° [5]; KEMAR
  fits 90°. Between them the error is small for both.
- **The band, 150 Hz to 4.5 kHz.** Below, an inverse needs a boost that a room's
  modes do not honour. Above, the head already separates the ears (−16 dB and
  falling on KEMAR), and no sphere describes a pinna.
- **The centre stays flat, the side path is bounded to ±6 dB.** A regularised
  inverse [12] with a flat mono path, so centred sound is not coloured.
- **The same length in milliseconds at every output rate,** so the rate does not
  change what the filter does at the bottom of its band.

### How we grade it

A filter always looks good on the model it was designed on: our first design
reported −16 dB on its own straight-line model and gained 2.6 dB on a real dummy
head. So 2.0.0 is graded on a head it does not know, the measured MIT KEMAR
responses [16], at the centre and 20 mm and 50 mm off it, against no filter at all.
The table is in [18](18-dsp-stages.md#xtc--crosstalk-cancellation). A dummy head in
an anechoic room is not your head in your room; the numbers say what the design
does, not what you will hear.

---

## 3. CUE: what it builds on

### Reading the pan

- **Per time–frequency region, from the channels' relation.** Estimating where a
  source is panned in each region of a short-time spectrum is established:
  Avendano & Jot's panning index [20, 21], DUET's histogram of level ratios and
  delays [22], and ADRess's frequency–azimuth plane [23].
- **The principal axis of a 2×2 covariance.** CUE reads the pan of each band from
  the principal axis of the covariance of L and R, and how far that axis stands
  out from the rest tells how much of the band is one coherent source and how much
  is diffuse. This is the primary–ambient decomposition of Goodwin & Jot [26] and
  the correlation-based ambience estimate of Merimaa, Goodwin & Jot [27].
- **ERB bands.** Analysis per equivalent rectangular bandwidth (Glasberg & Moore
  [28]): the resolution of the ear, not of the FFT. In the bass a band is read with
  its neighbours over 250 Hz, because one note's harmonics are one source.
- **The pan law.** −3 dB sin/cos by default, −4.5 and −6 dB as options. Which law a
  mix used is not written in it; the Calibrate button takes the places found so far
  as the mix's sources.

### Completing the cues

- **Time below f₁ (700 Hz by default).** A true delay of the lagging channel, at
  every frequency, so the attack moves with it and no echo appears. Below f₁ the
  head is Kuhn's low-frequency sphere [4], above f₂ Woodworth's ray [3], with a
  crossfade between. f₂ at 1.5 kHz sits at the limit of fine-structure ITD [2].
- **Level by rotation.** The level difference below f₁ is taken off, and the head
  shadow of a rigid sphere [5] is applied above f₂, by rotating the (L, R) pair
  rather than by gain. A rotation keeps the power of every bin, so on and off play
  at one level.
- **Envelope time above f₂.** Above the fine-structure limit the delay still moves
  the envelope, which the ear follows [3].
- **Stage width.** The angle a hard pan is given (30–90°). Time and shadow follow
  the same angle, so they stay one place.

### Leaving things alone

- **Recordings that already have time differences** (spaced microphones, binaural,
  ORTF) do not need the stage. A gate reads the generalised cross-correlation with
  phase transform between the channels (GCC-PHAT, Knapp & Carter [29]); a clear peak
  away from zero lag means real time differences are there. With mono or natural
  cues the stage plays the input bit for bit. A coincident (Blumlein) pair has no
  time difference to find, so it reads as pan-pot.
- **Diffuse bands and bands out of phase** are left as they are.

### The closest earlier work

| Work | What it does | How CUE differs |
|---|---|---|
| Goodwin & Jot 2007 [24]; Creative US 8,374,365 [30] | Direction per time–frequency component of a stereo or multichannel signal, then HRTF cues per component for headphones; the patent also names crosstalk cancellation for loudspeakers | CUE applies no HRTF set and no virtual speakers or room: only the time difference and a sphere's shadow for the same angle, power kept per band, natural recordings bit for bit |
| Breebaart & Schuijers 2008 [25] | Phantom sources of a stereo mix rendered binaurally for headphones | Same difference; CUE is also built to sit in front of a crosstalk canceller designed on the same head model |
| Avendano & Jot [20, 21] | Panning index and re-panning for upmix to more loudspeakers | CUE re-renders for two ears, not more speakers |

---

## 4. The Cue window

The window shows the stage as the listener faces it: each place the stage reads a
source at stands on a floor, its image running from low frequencies at the bottom
to high ones at the top, wider where it is louder, farther back the more of it is
diffuse.

- **Places** are peaks of a histogram of the bands' angles, the way DUET finds
  sources in a mixture [22]. Showing stereo position against frequency is known
  in analysers (ADRess [23]; stereo spectrogram displays in metering plugins).
- **Depth** is the direct share from the covariance reading [26, 27], an estimate,
  not a distance in metres.
- **Stage width ghosts** show where a place stood between the speakers and where
  the stage moves it.

---

## 5. What is ours

Each of these is a design and engineering decision of AuraEngine, not a scientific
discovery:

1. **Completing cues, not virtualising.** CUE does not render a source through an
   HRTF set, virtual speakers or a room. It adds only what a pan-pot leaves out,
   the time difference and the head shadow, for the angle the mix already gives,
   and keeps the power of every band.
2. **One head for both stages.** CUE and XTC use the same rigid-sphere head, and CUE
   comes in on XTC's own low edge: below it each ear hears both speakers, which give
   the cues by themselves.
3. **Doing nothing when it is not needed.** The GCC-PHAT gate and the bit-exact
   bypass: a natural recording, a mono track or the B key play the input sample
   for sample.
4. **Graded on a head the design does not know.** XTC alone, and CUE with XTC
   against a real source at the same angle, on measured KEMAR responses: the time
   difference error falls from 145 to 80 µs (see [18](18-dsp-stages.md)).
5. **Live, in the player, at any output rate,** with a window that shows what the
   stage reads.

As far as we checked, the crosstalk cancellers available to listeners (BACCH-SP,
BACCH4Mac, uBACCH, ambio.one, AmbiophonicDSP, foo_dsp_ambio) apply one filter to
the whole mix and do not read sources in it. We do not claim to be the first to do
any part of this.

---

## 6. How we check it

- **Measurement.** KEMAR responses [16], 10–90° pans, ITD at 200–1300 Hz, group
  delay at 200–1000 Hz, ILD; XTC per band with the head centred, 20 mm and 50 mm
  off.
- **Bit-exact tests.** Off, bypassed, mono and natural recordings: the output is
  the input sample for sample.
- **Listening.** A/B at the same level (key B). A pointing test with known pans is
  the next step: it measures what you hear, which a dummy head cannot.

What none of this covers: your own ears, your room's reflections, several
sources in one ERB band (they read as one place).

---

## References

1. Lord Rayleigh (J. W. Strutt), "On our perception of sound direction," *Philosophical Magazine* 13, 214–232 (1907).
2. A. Brughera, L. Dunai, W. M. Hartmann, "Human interaural time difference thresholds for sine tones: the high-frequency limit," *J. Acoust. Soc. Am.* 133(5), 2839–2855 (2013). https://pmc.ncbi.nlm.nih.gov/articles/PMC3663869
3. J. Blauert, *Spatial Hearing: The Psychophysics of Human Sound Localization*, rev. ed., MIT Press (1997). Woodworth's formula: R. S. Woodworth, *Experimental Psychology*, Holt (1938).
4. G. F. Kuhn, "Model for the interaural time differences in the azimuthal plane," *J. Acoust. Soc. Am.* 62(1), 157–167 (1977).
5. R. O. Duda, W. L. Martens, "Range dependence of the response of a spherical head model," *J. Acoust. Soc. Am.* 104(5), 3048–3058 (1998).
6. B. B. Bauer, "Stereophonic earphones and binaural loudspeakers," *J. Audio Eng. Soc.* 9(2), 148–151 (1961).
7. V. Pulkki, M. Karjalainen, "Localization of amplitude-panned virtual sources I: stereophonic panning," *J. Audio Eng. Soc.* 49(9), 739–752 (2001).
8. E. De Sena, Z. Cvetković, H. Hacıhabiboğlu, M. Moonen, T. van Waterschoot, "Localization uncertainty in time-amplitude stereophonic reproduction," *IEEE/ACM Trans. Audio, Speech, Lang. Process.* (2020), doi:10.1109/TASLP.2020.2975419. https://arxiv.org/abs/1907.11425
9. B. S. Atal, M. R. Schroeder, "Apparent sound source translator," US patent 3,236,949 (1966).
10. D. H. Cooper, J. L. Bauck, "Prospects for transaural recording," *J. Audio Eng. Soc.* 37(1/2), 3–19 (1989).
11. J. Bauck, D. H. Cooper, "Generalized transaural stereo and applications," *J. Audio Eng. Soc.* 44(9), 683–705 (1996).
12. O. Kirkeby, P. A. Nelson, H. Hamada, F. Orduna-Bustamante, "Fast deconvolution of multichannel systems using regularization," *IEEE Trans. Speech Audio Process.* 6(2), 189–194 (1998), doi:10.1109/89.661479.
13. E. Y. Choueiri, "Optimal crosstalk cancellation for binaural audio with two loudspeakers," Princeton University 3D3A Lab; US patent 9,167,344 (2015).
14. D. B. Ward, G. W. Elko, "Effect of loudspeaker position on the robustness of acoustic crosstalk cancellation," *IEEE Signal Process. Lett.* 6(5), 106–108 (1999), doi:10.1109/97.755428.
15. P. D. Hatziantoniou, J. N. Mourjopoulos, "Generalized fractional-octave smoothing of audio and acoustic responses," *J. Audio Eng. Soc.* 48(4), 259–280 (2000).
16. W. G. Gardner, K. D. Martin, "HRTF measurements of a KEMAR," *J. Acoust. Soc. Am.* 97(6), 3907–3908 (1995), doi:10.1121/1.412407; data: MIT Media Lab.
17. B. Masiero, X. Qiu, "Two listeners crosstalk cancellation system modelled by four point sources and two rigid spheres," journal article (2009). https://research-repository.rmit.edu.au/articles/journal_contribution/Two_listeners_crosstalk_cancellation_system_modelled_by_four_point_sources_and_two_rigid_spheres/27460650
18. E. M. Patros et al. (Google / Dysonics), "Optimal crosstalk cancellation filter sets generated by using an obstructed field model," US patent 11,589,162 (priority 2018).
19. V. R. Algazi, C. Avendano, R. O. Duda, "Estimation of a spherical-head model from anthropometry," *J. Audio Eng. Soc.* 49(6), 472–479 (2001).
20. C. Avendano, J.-M. Jot, "A frequency-domain approach to multichannel upmix," *J. Audio Eng. Soc.* 52(7/8), 740–749 (2004); first presented at the AES 22nd International Conference (2002).
21. C. Avendano, "Frequency-domain source identification and manipulation in stereo mixes for enhancement, suppression and re-panning applications," IEEE WASPAA, 55–58 (2003).
22. A. Jourjine, S. Rickard, Ö. Yılmaz, "Blind separation of disjoint orthogonal signals: demixing N sources from 2 mixtures," Proc. IEEE ICASSP (2000); Ö. Yılmaz, S. Rickard, "Blind separation of speech mixtures via time-frequency masking," *IEEE Trans. Signal Process.* 52(7) (2004).
23. D. Barry, B. Lawlor, E. Coyle, "Sound source separation: azimuth discrimination and resynthesis," Proc. DAFx-04, 240–244 (2004).
24. M. M. Goodwin, J.-M. Jot, "Binaural 3-D audio rendering based on spatial audio scene coding," AES 123rd Convention, paper 7277 (2007).
25. J. Breebaart, E. Schuijers, "Phantom materialization: a novel method to enhance stereo audio reproduction on headphones," *IEEE Trans. Audio, Speech, Lang. Process.* 16(8), 1503–1511 (2008).
26. M. M. Goodwin, J.-M. Jot, "Primary-ambient signal decomposition and vector-based localization for spatial audio coding and enhancement," Proc. IEEE ICASSP (2007).
27. J. Merimaa, M. M. Goodwin, J.-M. Jot, "Correlation-based ambience extraction from stereo recordings," AES 123rd Convention (2007).
28. B. R. Glasberg, B. C. J. Moore, "Derivation of auditory filter shapes from notched-noise data," *Hearing Research* 47(1–2), 103–138 (1990).
29. C. H. Knapp, G. C. Carter, "The generalized correlation method for estimation of time delay," *IEEE Trans. Acoust., Speech, Signal Process.* 24(4), 320–327 (1976).
30. M. M. Goodwin, J.-M. Jot, M. Dolson (Creative Technology), "Spatial audio analysis and synthesis for binaural reproduction and format conversion," US patent 8,374,365 (priority 2006).
