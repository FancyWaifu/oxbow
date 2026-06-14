/* A minimal curl on oxbow: fetch a URL with libcurl's easy interface and write
 * the body to stdout. `curl <url>` (default http://10.0.2.2:8080/). */
#include <stdio.h>
#include <string.h>
#include <curl/curl.h>

/* HTTP/3 disabled (vquic not built): stub the capability probe. */
int Curl_conn_may_http3(const void *data, const void *conn) { (void)data; (void)conn; return 1; }

static size_t write_cb(char *ptr, size_t size, size_t nmemb, void *userdata) {
    (void)userdata;
    size_t n = size * nmemb;
    fwrite(ptr, 1, n, stdout);
    return n;
}

int main(int argc, char **argv) {
    const char *url = (argc > 1) ? argv[1] : "http://10.0.2.2:8080/";
    curl_global_init(CURL_GLOBAL_DEFAULT);
    CURL *curl = curl_easy_init();
    if (!curl) {
        printf("curl: init failed\n");
        return 1;
    }
    curl_easy_setopt(curl, CURLOPT_URL, url);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, write_cb);
    curl_easy_setopt(curl, CURLOPT_HTTPGET, 1L);
    curl_easy_setopt(curl, CURLOPT_FOLLOWLOCATION, 1L);
    curl_easy_setopt(curl, CURLOPT_USERAGENT, "curl/oxbow");
    CURLcode res = curl_easy_perform(curl);
    if (res != CURLE_OK) {
        printf("\ncurl: (%d) %s\n", res, curl_easy_strerror(res));
    }
    curl_easy_cleanup(curl);
    curl_global_cleanup();
    return (res == CURLE_OK) ? 0 : 1;
}
