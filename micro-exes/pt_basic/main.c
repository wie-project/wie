#include <windows.h>
#include <pthread.h>
#include <stdlib.h>

static int counter;
static pthread_mutex_t mtx = PTHREAD_MUTEX_INITIALIZER;

void* worker(void* arg) {
    long id = (long)arg;
    for (int i = 0; i < 1000; i++) {
        pthread_mutex_lock(&mtx);
        counter++;
        pthread_mutex_unlock(&mtx);
    }
    return (void*)(id * 10);
}

int main() {
    pthread_t t1, t2;
    void* ret1;
    void* ret2;
    pthread_create(&t1, NULL, worker, (void*)1);
    pthread_create(&t2, NULL, worker, (void*)2);
    pthread_join(t1, &ret1);
    pthread_join(t2, &ret2);
    if (counter != 2000) return 1;
    if ((long)ret1 != 10 || (long)ret2 != 20) return 2;
    return 0;
}
