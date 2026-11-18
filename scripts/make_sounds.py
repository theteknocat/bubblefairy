#!/usr/bin/env python3
"""Synthesises BubbleFairy's notification sounds (original, so no licences to carry) into data/sounds/*.wav.

Run: python3 scripts/make_sounds.py   (standard library only; the outputs are committed)
"""
import math, os, random, struct, wave

RATE = 44100
OUT = os.path.join(os.path.dirname(__file__), "..", "data", "sounds")


def env(t, attack=0.004, decay=6.0):
    """Quick linear attack (no click), then an exponential fade."""
    return min(1.0, t / attack) * math.exp(-decay * t)


def note(buf, start, freq, dur, partials, decay, gain=1.0):
    """Add a struck tone made of (ratio, amplitude, extra decay) partials."""
    n0 = int(start * RATE)
    for i in range(int(dur * RATE)):
        t = i / RATE
        s = sum(a * math.sin(2 * math.pi * freq * r * t) * math.exp(-d * t) for r, a, d in partials)
        j = n0 + i
        if j < len(buf):
            buf[j] += gain * env(t, decay=decay) * s


def sweep(buf, start, f0, f1, dur, decay, gain=1.0):
    """A sine gliding from f0 to f1 over `dur` (a drop, a pop)."""
    n0, phase = int(start * RATE), 0.0
    for i in range(int(dur * RATE)):
        t = i / RATE
        f = f0 + (f1 - f0) * min(1.0, t / (dur * 0.5))
        phase += 2 * math.pi * f / RATE
        j = n0 + i
        if j < len(buf):
            buf[j] += gain * env(t, attack=0.002, decay=decay) * math.sin(phase)


def save(name, buf):
    peak = max(abs(x) for x in buf) or 1.0
    fade = int(0.01 * RATE)
    for i in range(fade):  # the tail ends at zero
        buf[-1 - i] *= i / fade
    with wave.open(os.path.join(OUT, name + ".wav"), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(b"".join(struct.pack("<h", int(32767 * 0.7 * x / peak)) for x in buf))


def blank(secs):
    return [0.0] * int(secs * RATE)


SOFT = [(1, 1.0, 0), (2, 0.25, 2), (3, 0.08, 4)]

# Chime: two rising notes
b = blank(0.9)
note(b, 0.00, 784.0, 0.6, SOFT, 5.5)
note(b, 0.13, 1174.7, 0.75, SOFT, 4.5, 0.9)
save("chime", b)

# Bell: inharmonic partials, long ring
b = blank(1.3)
note(b, 0, 880.0, 1.3, [(1, 1.0, 0), (2.0, 0.6, 1.5), (2.76, 0.45, 2.5), (5.4, 0.25, 4), (8.93, 0.12, 6)], 3.2)
save("bell", b)

# Marimba: short wooden plink pair
b = blank(0.7)
note(b, 0.00, 523.3, 0.45, [(1, 1.0, 0), (4, 0.35, 14), (9.2, 0.1, 30)], 7.0)
note(b, 0.11, 784.0, 0.5, [(1, 1.0, 0), (4, 0.35, 14), (9.2, 0.1, 30)], 7.0, 0.9)
save("marimba", b)

# Pop: a falling blip with a tiny click
random.seed(3)
b = blank(0.25)
sweep(b, 0, 1000.0, 260.0, 0.16, 22.0)
for i in range(120):
    b[i] += (random.random() - 0.5) * 0.15 * (1 - i / 120)
save("pop", b)

# Drop: a rising blip, like a water drop
b = blank(0.5)
sweep(b, 0, 380.0, 1100.0, 0.22, 12.0)
note(b, 0.02, 1650.0, 0.3, [(1, 0.25, 0)], 14.0)
save("drop", b)

# Ping: a high soft tone
b = blank(0.7)
note(b, 0, 1318.5, 0.65, [(1, 1.0, 0), (1.5, 0.2, 3), (3.0, 0.12, 6)], 6.0)
save("ping", b)
