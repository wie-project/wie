typedef unsigned long long u64;
typedef unsigned int u32;
#define NI __attribute__((noinline))
/* stack-resident ALU loop (long_loop shape) */
NI u64 k_alu(u64 n){ volatile u64 c=0; u64 r=0; while(c<n){ volatile u64 t=c^0xDEADBEEF; t=t*3+1; r+=t; c++; } return r; }
/* scale-8 SIB loads/stores (SIB addressing), cache resident */
NI u64 k_sib(long long *a,long long *b,u64 reps){ long long acc=0; for(u64 r=0;r<reps;r++) for(int i=0;i<4096;i++){ acc+=a[i]*3+b[i]; a[i]^=acc&0xff; } return (u64)acc; }
/* direct call/ret */
NI u64 k_fib(u64 n){ return n<2?n:k_fib(n-1)+k_fib(n-2); }
/* indirect calls through a table */
NI u64 f0(u64 x){return x+7;} NI u64 f1(u64 x){return x^0x5555;} NI u64 f2(u64 x){return x*3;} NI u64 f3(u64 x){return x>>1;}
NI u64 k_indirect(u64 (**tab)(u64),u64 n){ u64 x=1; for(u64 i=0;i<n;i++) x=tab[(x^i)&3](x)+i; return x; }
/* data-dependent branches */
NI u64 k_branch(u64 n){ u64 x=88172645463325252ULL,s=0; for(u64 i=0;i<n;i++){ x^=x<<13; x^=x>>7; x^=x<<17; if(x&1) s+=x>>3; else if(x&2) s^=x; else s-=i; } return s; }
/* scalar double arithmetic */
NI u64 k_fp(u64 n,double a,double b){ double x=1.0,s=0.0; for(u64 i=0;i<n;i++){ x=x*a+b; s+=x/(x+b); } return (u64)s; }
/* byte loads/stores through a lookup table (palette-blit shape) */
NI u64 k_bytes(unsigned char *src,u32 *pal,u32 *dst,u64 reps){ u64 s=0; for(u64 r=0;r<reps;r++){ for(int i=0;i<65536;i++) dst[i]=pal[src[i]]; s+=dst[r&65535]; } return s; }
/* pointer chase over 32 MiB: dominated by data-cache misses */
NI u64 k_chase(u32 *next,u64 n){ u32 p=0; for(u64 i=0;i<n;i++) p=next[p]; return p; }
