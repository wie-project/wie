#include <stdio.h>
#include <stdlib.h>
typedef unsigned long long u64; typedef unsigned int u32;
#ifdef _WIN32
#include <windows.h>
#define ABI
static double now(void){LARGE_INTEGER c,f;QueryPerformanceCounter(&c);QueryPerformanceFrequency(&f);return 1000.0*c.QuadPart/f.QuadPart;}
#else
#include <time.h>
#define ABI __attribute__((ms_abi))
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e3+t.tv_nsec/1e6;}
#endif
ABI u64 k_alu(u64); ABI u64 k_sib(long long*,long long*,u64); ABI u64 k_fib(u64);
ABI u64 f0(u64); ABI u64 f1(u64); ABI u64 f2(u64); ABI u64 f3(u64);
ABI u64 k_indirect(ABI u64(**)(u64),u64); ABI u64 k_branch(u64); ABI u64 k_fp(u64,double,double);
ABI u64 k_bytes(unsigned char*,u32*,u32*,u64); ABI u64 k_chase(u32*,u64);
static long long a[4096],b[4096]; static unsigned char src[65536]; static u32 pal[256],dst[65536];
#define N (8u<<20)
static char *num(char *p,u64 v){char t[24];int n=0;do{t[n++]='0'+v%10;v/=10;}while(v);while(n)*p++=t[--n];return p;}
static void emit(const char*name,int pass,u64 cs,u64 r){char buf[128],*p=buf;while(*name)*p++=*name++;*p++=' ';*p++='p';*p++='a';*p++='s';*p++='s';p=num(p,pass);*p++=' ';p=num(p,cs/100);*p++='.';*p++='0'+(cs/10)%10;*p++='0'+cs%10;*p++=' ';p=num(p,r);*p++='\n';
#ifdef _WIN32
 DWORD w;WriteFile(GetStdHandle(STD_OUTPUT_HANDLE),buf,(DWORD)(p-buf),&w,0);
#else
 fwrite(buf,1,p-buf,stdout);
#endif
}
static const char *only;
static int want(const char*n){const char*a=only,*b=n;if(!a)return 1;while(*a&&*a==*b){a++;b++;}return !*a&&!*b;}
#define T(name,expr) do{ if(want(name)) for(int pass=0;pass<2;pass++){ double t0=now(); u64 r=(expr); double t1=now(); emit(name,pass,(u64)((t1-t0)*100.0),r);} }while(0)
int main(int argc,char**argv){
  only=argc>1?argv[1]:0;
  for(int i=0;i<4096;i++){a[i]=i;b[i]=i^0x5a5a;}
  for(int i=0;i<65536;i++)src[i]=(unsigned char)(i*31); for(int i=0;i<256;i++)pal[i]=i*0x010203u;
  u32 *next=malloc(N*sizeof *next); for(u32 i=0;i<N;i++)next[i]=i;
  u64 x=12345; for(u32 i=N-1;i>0;i--){x^=x<<13;x^=x>>7;x^=x<<17; u32 j=x%i,t=next[i];next[i]=next[j];next[j]=t;}
  ABI u64 (*tab[4])(u64)={f0,f1,f2,f3};
  T("alu",k_alu(50000000));
  T("sib",k_sib(a,b,6000));
  T("fib",k_fib(36));
  T("indirect",k_indirect(tab,30000000));
  T("branch",k_branch(50000000));
  T("fp",k_fp(30000000,0.999999,0.5));
  T("bytes",k_bytes(src,pal,dst,1500));
  T("chase",k_chase(next,3000000));
  return 0;
}
