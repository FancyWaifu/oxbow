/* ffi-test — call functions through libffi's ffi_call with dynamically built
 * call interfaces, the way libwayland dispatches a wire message to a handler. */
#include <stdio.h>
#include <ffi.h>

static int add_ints(int a, int b) { return a + b; }
static double scale(double x, int n) { return x * n; }
static const char *pick(const char *a, const char *b, int which)
{
  return which ? b : a;
}

int main(void)
{
  printf("[ffi-test] libffi dynamic calls\n");
  int ok = 1;

  /* 1. int add_ints(int, int) */
  {
    ffi_cif   cif;
    ffi_type *args[2] = { &ffi_type_sint, &ffi_type_sint };
    if (ffi_prep_cif(&cif, FFI_DEFAULT_ABI, 2, &ffi_type_sint, args) != FFI_OK) {
      printf("[ffi-test] prep_cif(add) failed\n");
      return 1;
    }
    int a = 3, b = 4;
    void *vals[2] = { &a, &b };
    ffi_arg r;
    ffi_call(&cif, FFI_FN(add_ints), &r, vals);
    printf("[ffi-test] add_ints(3,4) = %d\n", (int)r);
    ok &= ((int)r == 7);
  }

  /* 2. double scale(double, int) — exercises SSE + integer arg classes */
  {
    ffi_cif   cif;
    ffi_type *args[2] = { &ffi_type_double, &ffi_type_sint };
    if (ffi_prep_cif(&cif, FFI_DEFAULT_ABI, 2, &ffi_type_double, args) != FFI_OK) {
      printf("[ffi-test] prep_cif(scale) failed\n");
      return 1;
    }
    double x = 2.5;
    int    n = 6;
    void  *vals[2] = { &x, &n };
    double r;
    ffi_call(&cif, FFI_FN(scale), &r, vals);
    printf("[ffi-test] scale(2.5,6) = %d (x10)\n", (int)(r * 10));
    ok &= ((int)(r * 10) == 150);
  }

  /* 3. pointer args + pointer return (what libwayland dispatch looks like) */
  {
    ffi_cif   cif;
    ffi_type *args[3] = { &ffi_type_pointer, &ffi_type_pointer, &ffi_type_sint };
    if (ffi_prep_cif(&cif, FFI_DEFAULT_ABI, 3, &ffi_type_pointer, args) != FFI_OK) {
      printf("[ffi-test] prep_cif(pick) failed\n");
      return 1;
    }
    const char *a = "wayland", *b = "oxbow";
    int         which = 1;
    void       *vals[3] = { &a, &b, &which };
    void       *r;
    ffi_call(&cif, FFI_FN(pick), &r, vals);
    printf("[ffi-test] pick -> %s\n", (const char *)r);
    ok &= (r == (void *)b);
  }

  printf("[ffi-test] %s\n", ok ? "OK: ffi_call works" : "FAIL");
  return ok ? 0 : 1;
}
