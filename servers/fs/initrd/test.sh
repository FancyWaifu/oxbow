echo hello from dash on oxbow
echo arithmetic: $((6 * 7))
x=5
echo var: $x
for i in a b c; do echo "item $i"; done
if true; then echo if-works; fi
echo cmdsub-builtin: $(echo inner-ok)
greet=$(echo world); echo nested-var: hi-$greet
while [ "$x" -gt 3 ]; do echo "count $x"; x=$((x - 1)); done
case foo in foo) echo case-ok;; esac
