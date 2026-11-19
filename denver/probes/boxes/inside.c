void _start(void){__asm__ volatile("syscall"::"a"(1),"D"(1),"S"("INNER_EXEC_PASS\n"),"d"(16):"rcx","r11","memory");__asm__ volatile("syscall"::"a"(60),"D"(0):"rcx","r11","memory");}
