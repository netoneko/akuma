#include <stdio.h>
#include <stdint.h>
static uint64_t cmpf(uint64_t a, uint64_t b, int size){
  uint64_t f;
  switch(size){
   case 1: __asm__ volatile("cmpb %2,%1; pushfq; popq %0":"=r"(f):"q"((uint8_t)a),"q"((uint8_t)b):"cc"); break;
   case 2: __asm__ volatile("cmpw %2,%1; pushfq; popq %0":"=r"(f):"r"((uint16_t)a),"r"((uint16_t)b):"cc"); break;
   case 4: __asm__ volatile("cmpl %2,%1; pushfq; popq %0":"=r"(f):"r"((uint32_t)a),"r"((uint32_t)b):"cc"); break;
   default: __asm__ volatile("cmpq %2,%1; pushfq; popq %0":"=r"(f):"r"(a),"r"(b):"cc"); break;
  }
  return f & 0x8D5;
}
static uint64_t testf(uint64_t a, uint64_t b, int size){
  uint64_t f;
  switch(size){
   case 1: __asm__ volatile("testb %2,%1; pushfq; popq %0":"=r"(f):"q"((uint8_t)a),"q"((uint8_t)b):"cc"); break;
   case 2: __asm__ volatile("testw %2,%1; pushfq; popq %0":"=r"(f):"r"((uint16_t)a),"r"((uint16_t)b):"cc"); break;
   case 4: __asm__ volatile("testl %2,%1; pushfq; popq %0":"=r"(f):"r"((uint32_t)a),"r"((uint32_t)b):"cc"); break;
   default: __asm__ volatile("testq %2,%1; pushfq; popq %0":"=r"(f):"r"(a),"r"(b):"cc"); break;
  }
  return f & 0x8D5 & ~0x10; /* AF undefined for test */
}
int main(void){
  uint64_t v[] = {0,1,2,0x7f,0x80,0xff,0x7fff,0x8000,0xffff,0x7fffffff,0x80000000ull,0xffffffffull,0x7fffffffffffffffull,0x8000000000000000ull,0xffffffffffffffffull,0x10,0x0f,0x1f,0x3,0x55aa};
  int n = sizeof v/sizeof v[0], sizes[]={1,2,4,8};
  for(int s=0;s<4;s++) for(int i=0;i<n;i++) for(int j=0;j<n;j++){
    printf("(%d, %#llxu64, %#llxu64, %#llxu64, %#llxu64),\n", sizes[s], (unsigned long long)v[i], (unsigned long long)v[j], (unsigned long long)cmpf(v[i],v[j],sizes[s]), (unsigned long long)testf(v[i],v[j],sizes[s]));
  }
}
