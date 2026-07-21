StrStack
========

A string allocation library. This is primarily useful when you want to allocate
a bunch of small strings, use them, and then destroy them all together.

Documentation
-------------

https://docs.rs/str_stack/latest/str_stack/

Performance
-----------

* Allocation: ~2.5x speedup (for 1000 strings) (~42ns per string)
* Indexing: 0.73x speedup (slower) (~1.7ns per index)
* Iterate: 0.35x speedup (much slower) (~1ns per iteration)
