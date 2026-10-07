#!/bin/sh
# Upper-cases standard input: an executable input file the farm must keep executable.
exec tr "[:lower:]" "[:upper:]"
