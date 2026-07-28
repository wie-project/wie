#include <windows.h>
#include <pthread.h>

static pthread_mutex_t mtx = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int ready;

void* waiter(void* arg) {
    (void)arg;
    pthread_mutex_lock(&mtx);
    while (!ready) pthread_cond_wait(&cv, &mtx);
    pthread_mutex_unlock(&mtx);
    return NULL;
}

int main() {
    pthread_t t;
    pthread_create(&t, NULL, waiter, NULL);
    pthread_mutex_lock(&mtx);
    ready = 1;
    pthread_cond_signal(&cv);
    pthread_mutex_unlock(&mtx);
    pthread_join(t, NULL);
    return 0;
}
