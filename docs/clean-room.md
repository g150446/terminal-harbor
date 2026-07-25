# Independent implementation boundary

Terminal Harbor is an MIT-licensed fork of WezTerm. Its workspace sidebar and
agent activity model are implemented from product requirements and WezTerm's
existing public mux and terminal APIs.

The following boundary applies to Terminal Harbor contributions:

- Do not copy or translate cmux source code, tests, assets, strings, constants,
  or internal documentation.
- Product-level ideas such as a workspace list, activity indicators, and remote
  observation may be implemented independently.
- New work should use Terminal Harbor terminology, data models, visual tokens,
  and protocols.
- If code is ported from another project, record its source and license in the
  commit and update `NOTICE` when attribution is required.

cmux is not a build-time or runtime dependency of Terminal Harbor. Its GPL
license therefore does not apply to this independent code merely because the
products share high-level features. This document records engineering policy,
not legal advice.
