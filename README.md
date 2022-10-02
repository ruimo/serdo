# Serdo

Serializable do/undo library.

## Abstract

- An implementation of GoF command pattern.
- By making your command serializable, you can serialize all undo information into files. And you can undo/redo operations even if you restart you application.
- You can save certain state as snapshot. You can restore the state from snapshots.

Fig. 1 illustrates the architecture. The application calls do() with undo information. 

![Abstract](figures/abstract.drawio.svg)

Fig. 1 Abstract
