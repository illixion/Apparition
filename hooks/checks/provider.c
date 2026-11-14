int ap_fixture_value(void) { return 7; }
__attribute__((noinline)) int ap_entry_value(void) {
    __asm__ volatile("nop\nnop\nnop\nnop\nnop\nnop\nnop\nnop");
    return 11;
}
