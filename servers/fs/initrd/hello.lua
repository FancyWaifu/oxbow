-- A Lua script loaded from the oxbow filesystem.  Run with:  lua /hello.lua
print("hello from a .lua file on oxbow!")

-- a closure-based counter
local function counter()
  local n = 0
  return function() n = n + 1; return n end
end
local next = counter()
print("counter:", next(), next(), next())

-- numeric for + string.rep
local line = string.rep("=", 24)
print(line)
for i = 1, 5 do
  print(string.format("  %d squared is %d, sqrt is %.3f", i, i * i, (i * i) ^ 0.5))
end
print(line)

-- a table acting as a record, with a method
local p = { x = 3, y = 4 }
function p:dist() return (self.x * self.x + self.y * self.y) ^ 0.5 end
print(string.format("distance of (%d,%d) from origin = %.1f", p.x, p.y, p:dist()))
