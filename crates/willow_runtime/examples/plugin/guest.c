/* Static Linux protocol example and adversarial fixture. Build with cc -static.
 * Headers are byte arrays, not native structs. Only owned bytes cross the pipe. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <errno.h>
static unsigned char header[48], payload[65536];
static uint64_t get(unsigned at, unsigned n) {
    uint64_t value = 0;
    for (unsigned i = 0; i < n; ++i) value |= (uint64_t)header[at+i] << (i*8);
    return value;
}
static void put(unsigned at, unsigned n, uint64_t value) {
    for (unsigned i = 0; i < n; ++i) header[at+i] = value >> (i*8);
}
static int read_all(void *buffer, size_t length) {
    unsigned char *p = buffer;
    while (length) { ssize_t n = read(0, p, length); if (n <= 0) return 0; p += n; length -= n; }
    return 1;
}
static void write_all(const void *buffer, size_t length) {
    const unsigned char *p = buffer;
    while (length) { ssize_t n = write(1, p, length); if (n <= 0) _exit(1); p += n; length -= n; }
}
int main(void) {
    memcpy(header, "WPLG", 4); put(4,4,1); put(8,4,2); write_all(header,48);
    while (read_all(header,48)) {
        uint64_t length = get(40,8), method = get(12,4);
        if (memcmp(header,"WPLG",4) || get(4,4)!=1 || length>sizeof(payload)) return 2;
        if (get(8,4)==4) continue;
        if (get(8,4)!=1 || !read_all(payload,length)) return 2;
        put(8,4,2);
        if (method==2) _exit(7);
        if (method==3) for (;;) { }
        if (method==4) {
            /* Real escape attempts: host sentinel read/write, network, fork,
               environment, and creating a writable file in the root. */
            if (length>=sizeof(payload)) return 3;
            payload[length]=0;
            int rd=open((char *)payload,O_RDONLY), wr=open((char *)payload,O_WRONLY);
            int net=socket(AF_INET,SOCK_STREAM,0);
            long child=syscall(SYS_clone,17,0,0,0,0);
            int root=open("/escape",O_CREAT|O_WRONLY,0600);
            unsigned mask=(rd<0) | ((wr<0)<<1) | ((net<0)<<2) | ((child<0)<<3)
                | ((getenv("HOME")==NULL)<<4) | ((root<0)<<5);
            if (rd>=0) close(rd); if (wr>=0) close(wr); if (net>=0) close(net); if(root>=0)close(root);
            payload[0]=mask; length=1;
        }
        if (method==5) {
            char path[64]="/resources/"; unsigned at=11; uint64_t id=get(32,8);
            if(id>9) return 4;
            path[at++]=(char)('0'+id);path[at]=0;
            int fd=open(path,O_RDONLY); if(fd<0) {put(8,4,3);payload[0]=3;length=1;}
            else {ssize_t n=read(fd,payload,sizeof(payload));close(fd);if(n<0)return 5;length=n;}
        }
        if (method==6) { put(40,8,UINT64_MAX); write_all(header,48); continue; }
        if (method==7) { put(24,8,get(24,8)+1); }
        if (method==8) { put(16,8,get(16,8)-1); }
        if (method==9) { put(4,4,99); }
        if (method==10) { write_all(header,10); _exit(0); }
        if (method==11) { put(8,4,99); }
        if (method==12) {
            void *p=mmap(NULL,512u*1024*1024,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
            payload[0]=(p==MAP_FAILED); length=1;
            if(p!=MAP_FAILED)munmap(p,512u*1024*1024);
        }
        if (method==16) {
            if(length!=4)return 6;
            int fd=(int)((unsigned)payload[0] | ((unsigned)payload[1]<<8) | ((unsigned)payload[2]<<16) | ((unsigned)payload[3]<<24));
            unsigned char probe;
            payload[0]=(pread(fd,&probe,1,0)<0 && errno==EBADF);length=1;
        }
        if(method==19)put(12,4,99);
        if(method==20)put(32,8,get(32,8)+1);
        if(method==21){put(40,8,10);write_all(header,48);return 0;}
        if(method>=22 && method<=24){put(8,4,3);payload[0]=method==22?1:method==23?2:99;length=1;}
        if(method==18) {
            unsigned count=0;struct stat info;
            for(int fd=3;fd<1024;++fd)if(fstat(fd,&info)==0)++count;
            payload[0]=(unsigned char)count;length=1;
        }
        put(40,8,length);
        if(method==14 || method==15) {
            put(8,4,5);
            unsigned count=method==14?1:17;
            for(unsigned i=0;i<count;++i){write_all(header,48);write_all(payload,length);}
            put(8,4,2);put(40,8,0);length=0;
        }
        if(method==13) {
            for(unsigned i=0;i<48;++i)write_all(header+i,1);
            for(uint64_t i=0;i<length;++i)write_all(payload+i,1);
        } else {write_all(header,48);write_all(payload,length);}
        if(method==17)return 0;
    }
    return 0;
}
