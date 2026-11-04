// Tiny macOS guest -> VZ host status sender. No credentials, no private entitlements.
#include <sys/socket.h>
#include <sys/vsock.h>
#include <sys/time.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
int main(void) {
    char data[8192]; size_t size=0;
    for (;;) {
        ssize_t n=read(STDIN_FILENO,data+size,sizeof(data)-size);
        if (n<0 && errno==EINTR) continue;
        if (n<0) return 1;
        if (n==0) break;
        size+=(size_t)n;
        if(size==sizeof(data)) return 1;
    }
    int fd=socket(AF_VSOCK,SOCK_STREAM,0);
    if(fd<0) return 1;
    struct timeval timeout={5,0};
    setsockopt(fd,SOL_SOCKET,SO_SNDTIMEO,&timeout,sizeof(timeout));
    setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout));
    struct sockaddr_vm address;
    memset(&address,0,sizeof(address));
    address.svm_len=sizeof(address); address.svm_family=AF_VSOCK;
    address.svm_cid=VMADDR_CID_HOST; address.svm_port=3939;
    if(connect(fd,(struct sockaddr*)&address,sizeof(address))<0) { close(fd);return 1; }
    size_t offset=0;
    while(offset<size) {
        ssize_t n=write(fd,data+offset,size-offset);
        if(n<0 && errno==EINTR) continue;
        if(n<=0) { close(fd); return 1; }
        offset+=(size_t)n;
    }
    shutdown(fd,SHUT_WR);
    char action=0;
    if(read(fd,&action,1)==1) {
        if(action=='S') write(STDOUT_FILENO,"shutdown",8);
        else if(action=='R') write(STDOUT_FILENO,"reboot",6);
    }
    close(fd);return 0;
}
