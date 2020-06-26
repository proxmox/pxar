API cleanup
===========

* Should probably find a way to better organize all the "open from X" methods.
* The encoder should probably track the current full path, which should then be added to
  the `LinkOffset` type, so add_hardlink doesn't take a target path anymore as that allows for
  inconsistencies between the offset and path.
