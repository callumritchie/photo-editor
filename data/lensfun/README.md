# Lens database

The XML files in this folder are the lens database of the Lensfun project, copied **unmodified** from
<https://github.com/lensfun/lensfun> (`data/db`, commit `37238fcef563196fb798f6151cb9023968b26c40`).

- Copyright © the Lensfun contributors.
- Licensed under the **Creative Commons Attribution-ShareAlike 3.0** licence (CC BY-SA 3.0); the
  full text is in `LICENSE-CC-BY-SA-3.0.txt` and at <https://creativecommons.org/licenses/by-sa/3.0/>.

These files are data distributed alongside this program, not part of its source code. They are
under their own licence, not the GPL that covers the rest of the repository. The program reads them
at run time (`crates/lens`) to correct distortion, chromatic aberration and vignetting for
calibrated lenses. To update, replace the XML files with newer ones from the same source.
