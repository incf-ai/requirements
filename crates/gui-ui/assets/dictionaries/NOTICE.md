# `en_US` dictionary provenance

`en_US.aff` and `en_US.dic` are the prebuilt Hunspell "en_US" (SCOWL size 60)
dictionary release `2015.05.18`, downloaded from:

<https://sourceforge.net/projects/wordlist/files/speller/2015.05.18/hunspell-en_US-2015.05.18.zip/>

**One line modified** from that download, per the Ispell/BSD license's "all
modifications must be clearly marked" term (`README_en_US.txt` lines 291-294):
`en_US.aff`'s first line was changed from `SET UTF8` to `SET UTF-8` — real
Hunspell accepts the former loosely, but the `zspell` crate's stricter parser
(`Encoding::try_from`) only recognizes the latter, hyphenated form. `en_US.dic`
is unmodified.

(source project: <https://github.com/en-wl/wordlist>, aka SCOWL/"Spell Checking
Oriented Word Lists" by Kevin Atkinson). The full upstream README, with complete
credits and per-sub-list copyright notes, is kept alongside these files as
`README_en_US.txt`.

## License

Deliberately picked over the LibreOffice/Mozilla `en_US` Hunspell dictionary
(tri-licensed MPL/GPL/LGPL) because this SCOWL-direct release is permissively
licensed with no copyleft/share-alike terms:

- The compiled word list is Copyright 2000-2015 Kevin Atkinson, under a
  BSD/MIT-style permissive grant ("Permission to use, copy, modify, distribute
  and sell these word lists... for any purpose... without fee, provided that
  the above copyright notice appears in all copies... provided 'as is' without
  warranty" — `README_en_US.txt` lines 82-94).
- The affix file is a modified version of Geoff Kuenning's Ispell `english.aff`,
  under his 3-clause-style BSD license (`README_en_US.txt` lines 279-311).
- At this dictionary's size level (60, not the "-large" 70/80 variants), every
  contributing sub-list `README_en_US.txt` documents is itself public domain
  (MWords/Moby, 12Dicts, ENABLE, Brian Kelk's UK wordlist) or under the same
  permissive WordNet/Princeton notice-preservation terms — nothing GPL/LGPL/MPL
  is pulled in.

Redistribution only requires keeping the copyright/permission notices intact,
which this file plus the retained `README_en_US.txt` do.
